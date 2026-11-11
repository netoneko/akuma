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

use akuma_cpu::msr::{self, Msr};
use akuma_syscalls_abi::Syscall;

mod dispatch;
// `pub` for the boot suite (`crate::testing::usermode`), which drives it with kernel buffers.
pub use dispatch::syscall_dispatch;

// The boot suite's ring-3 tests are `crates/akuma-kernel-tests-amd64/src/usermode.rs`; what
// they reach in here is `pub` and re-exported through `crate::testing::usermode` only.
#[cfg(not(feature = "no-tests"))]
pub use dispatch::to_glue;

use akuma_mmap::{MmapRegion, PhysFrame};
use alloc::vec::Vec;

use akuma_exec::process::UserContext;
use akuma_mmu::{LeafAction, PteProt, UserAddressSpace};

use crate::loader;
use crate::serial;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// `nopidfd` on the kernel command line: `pidfd_open` answers `ENOSYS`, the way a
/// kernel older than 5.3 would, so a program that probes for it takes its
/// fallback path. A diagnostic for the `/login` input stall
/// (`docs/archive/AMD64_CLAUDE_LOGIN_INPUT_STUCK.md`), never a default.
pub static NO_PIDFD: AtomicBool = AtomicBool::new(false);
use spinning_top::Spinlock;

/// `EFER.SCE` — without it, `syscall` raises `#UD`.
const EFER_SCE: u64 = 1 << 0;


/// Top of the stack given to an ELF-loaded process.
///
/// Near the ceiling of the lower half rather than just above the image, which is
/// where the hand-assembled program's stack goes: an ELF decides its own extent,
/// and `0x41_0000` is inside `hello`'s. Linux puts the stack at the top of the
/// user address space for the same reason — it is the one place a program's
/// segments cannot already be.
pub const ELF_STACK_TOP: u64 = 0x7FFF_FFFF_F000;
/// Pages of stack.
///
/// The initial frame is under 200 bytes, but the stack is
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

/// How far the main stack may grow: 8 MiB, Linux's default `RLIMIT_STACK`.
///
/// Only [`ELF_STACK_PAGES`] of it are mapped at exec; the rest below is one
/// anonymous demand-paged region ([`crate::mm::lazy_stack_region`]), so a
/// process pays for the pages it touches. Without it the stack ended hard at
/// 512 KiB: coreutils' `wc -l` keeps a 256 KiB buffer on the stack, ran off the
/// bottom, and died on a `#PF` with `cr2 == rsp` that nothing serviced
/// (2026-10-15, `docs/archive/AKUMA_GENTOO.md`). The same shape is a likely
/// suspect for the Python `_blake2` crash recorded in
/// `AKUMA_GENTOO_PORTAGE_CRASH.md`.
pub const ELF_STACK_MAX_PAGES: usize = 2048;





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
            proc_slot: usize::MAX,
            user_rip: 0,
            fs_base: 0,
            saved_regs: [0; 12],
            gs_base: 0,
            thread_slot: crate::thread::NO_THREAD,
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

/// `AF_UNIX`'s value in `socket(2)`'s `domain` — the one family this target
/// serves from `akuma-syscalls-glue` rather than `crate::sock`.
pub const AF_UNIX: u64 = 1;

/// How many syscalls arrived.
static CALLS: AtomicU64 = AtomicU64::new(0);

/// Print every syscall number and its result.
///
/// Off during the self-tests, turned
/// on by `run_init` when `strace` is on the command line — a bring-up aid for
/// running a program the tree did not compile (busybox).
pub static SYSCALL_TRACE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Print only the syscalls that **fail** — one `[sc!]` line each, with the
/// pid, the decoded path arguments and the errno. Turned on by `strace_err` on
/// the command line.
///
/// Added 2026-10-08 for Chromium, whose startup is ~100 000 syscalls across
/// ~30 processes: [`SYSCALL_TRACE`]'s two lines per call are more serial time
/// than the run has, and the question was never "what did it call" but "which
/// call said `ENOENT`, in which process, about which path". One line per
/// failure, written in one flush so two cores cannot shred it.
pub static SYSCALL_TRACE_ERRORS: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Restrict [`SYSCALL_TRACE`] to one thread group: `strace_pid=<n>` on the
/// command line (which also turns the trace on). `0` traces everyone.
///
/// The full trace over a whole Chromium run is ~200 k lines; the browser
/// process alone, in the second before it crashes, is what the question needs
/// once `[sc!]` has named the process.
pub static SYSCALL_TRACE_PID: AtomicU64 = AtomicU64::new(0);

/// Syscall numbers whose every call — success included — gets an `[sc!]` line,
/// one bit per number below 512. Set from `strace_nr=` on the command line
/// ([`trace_nrs_from`]); implies nothing about [`SYSCALL_TRACE_ERRORS`].
///
/// The errors-only trace cannot see a call that *succeeds with the wrong
/// answer*, and that is the shape Chromium's crashpad failure turned out to
/// have: `execve("chrome_crashpad_handler")` failed because some earlier call
/// had returned a path without its directory, without failing.
static SYSCALL_TRACE_NRS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

/// Parse `strace_nr=`'s comma-separated list into [`SYSCALL_TRACE_NRS`].
/// Numbers that do not parse, or are 512 or more, are ignored.
pub fn trace_nrs_from(list: &str) {
    for n in list.split(',').filter_map(|t| t.parse::<u64>().ok()).filter(|&n| n < 512) {
        SYSCALL_TRACE_NRS[(n / 64) as usize].fetch_or(1 << (n % 64), Ordering::Relaxed);
    }
}

fn trace_nr_selected(nr: u64) -> bool {
    nr < 512 && SYSCALL_TRACE_NRS[(nr / 64) as usize].load(Ordering::Relaxed) & (1 << (nr % 64)) != 0
}

/// The path-carrying argument(s) of `nr`, as `(first, second)` user pointers.
///
/// One table for both traces, so a syscall cannot be decoded in one and not
/// the other. An `*at` call's dirfd is its `a1`, which both lines print.
fn trace_paths(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> (Option<u64>, Option<u64>) {
    match nr {
        // open, stat, lstat, access, execve, chdir, rename, mkdir, rmdir,
        // creat, link, unlink, symlink, readlink, chmod, chown, lchown,
        // truncate, statfs, chroot, mknod, utimes
        2 | 4 | 6 | 21 | 59 | 80 | 83 | 84 | 85 | 87 | 89 | 90 | 92 | 94 | 76 | 137
        | 161 | 133 | 235 => (Some(a1), None),
        // rename, link, symlink (target, linkpath)
        82 | 86 | 88 => (Some(a1), Some(a2)),
        // openat, mkdirat, mknodat, fchownat, newfstatat, unlinkat, readlinkat,
        // fchmodat, faccessat, utimensat, execveat, faccessat2, statx
        257 | 258 | 259 | 260 | 262 | 263 | 267 | 268 | 269 | 280 | 322 | 439 | 332 => {
            (Some(a2), None)
        }
        // renameat, linkat, renameat2 (old, new)
        264 | 265 | 316 => (Some(a2), Some(a4)),
        // symlinkat (target, newdirfd, linkpath)
        266 => (Some(a1), Some(a3)),
        _ => (None, None),
    }
}

/// The user string at `ptr` (ASCII prefix, at most 255 bytes) copied into
/// `buf`, as a `&str` for a print. A bad pointer reads as empty. `read_cstr`
/// allocates (bounded, 256 B) — acceptable on an opt-in trace, and the helper
/// `fd::trace_user_cstr` has always used.
fn trace_cstr(ptr: u64, buf: &mut [u8; 256]) -> &str {
    let n = crate::uaccess::read_cstr(ptr, 256).map_or(0, |s| {
        let n = s.iter().take_while(|b| b.is_ascii()).count().min(buf.len());
        buf[..n].copy_from_slice(&s[..n]);
        n
    });
    core::str::from_utf8(&buf[..n]).unwrap_or("")
}

/// The `[sc!]` line for a failed syscall — see [`SYSCALL_TRACE_ERRORS`] — or
/// for any call of a number named by `strace_nr=` ([`SYSCALL_TRACE_NRS`]).
/// A successful `readlink`/`readlinkat` also shows the target it returned.
///
/// One `tprint!`, so the line carries the uptime stamp other traces are
/// ordered by and two cores cannot shred it. Every argument is decoded into a
/// stack buffer first.
fn trace_error(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, r: u64) {
    let pid = current_process().map_or(0, |p| p.pid);
    let task = crate::sched::current_task();
    let failed = r >= 0u64.wrapping_sub(4095);
    let (p1, p2) = trace_paths(nr, a1, a2, a3, a4);
    let (mut b1, mut b2, mut b3) = ([0u8; 256], [0u8; 256], [0u8; 256]);
    let s1 = p1.map_or("", |p| trace_cstr(p, &mut b1));
    let s2 = p2.map_or("", |p| trace_cstr(p, &mut b2));
    // The bytes `readlink` wrote: not NUL-terminated, exactly `r` of them.
    let target = match (nr, failed) {
        (89, false) => Some(a2),
        (267, false) => Some(a3),
        _ => None,
    }
    .and_then(|ptr| {
        let n = (r as usize).min(b3.len());
        crate::uaccess::read_bytes(ptr, &mut b3[..n]).then_some(n)
    })
    .map_or("", |n| {
        let n = b3[..n].iter().take_while(|c| c.is_ascii()).count();
        core::str::from_utf8(&b3[..n]).unwrap_or("")
    });
    let (sign, val) = if failed { ("-", r.wrapping_neg()) } else { ("", r) };
    akuma_primitives::tprint!(
        1024,
        "[sc!] pid={pid} task={task} nr={nr} a1=0x{a1:x} a2=0x{a2:x} a3=0x{a3:x} -> {sign}{val}{}{s1}{}{}{s2}{}{}{target}{}\n",
        if p1.is_some() { " \"" } else { "" },
        if p1.is_some() { "\"" } else { "" },
        if p2.is_some() { " -> \"" } else { "" },
        if p2.is_some() { "\"" } else { "" },
        if target.is_empty() { "" } else { " = \"" },
        if target.is_empty() { "" } else { "\"" },
    );
}
/// Bytes accepted by `write`, across all processes.
static WRITTEN: AtomicU64 = AtomicU64::new(0);

/// Which task performed each `write`, in order.
///
/// Proving that two processes ran
/// concurrently needs the *interleaving*, not just the totals: three writes from
/// A followed by three from B would satisfy every count-based check and would
/// mean the scheduler never switched.
pub static WRITE_SEQ: [AtomicU64; 32] = [const { AtomicU64::new(u64::MAX) }; 32];
pub static WRITE_SEQ_LEN: AtomicU64 = AtomicU64::new(0);
/// Which *core* served each `write`, alongside [`WRITE_SEQ`].
///
/// Two processes
/// whose writes arrive from two cores ran on two cores — the ring-3 half of
/// the SMP self-test, and the only witness of user-mode parallelism the kernel
/// can take without instrumenting the programs.
pub static WRITE_CPU: [AtomicU64; 32] = [const { AtomicU64::new(u64::MAX) }; 32];

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
"#,
    // `.Lsig_return`'s `iretq` selectors: the same ring-3 rows `sysretq`
    // derives from `IA32_STAR`, taken from the GDT crate so they cannot drift.
    user_cs = const crate::gdt::USER_CS as u64,
    user_ss = const crate::gdt::USER_SS as u64,
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

/// Leave ring 3 from an exception handler because signal `sig`'s default
/// action says to, as if the program had called `exit_group(-sig)`.
///
/// The syscall path's `.Lexit_to_kernel`, done by hand: back onto the task's
/// kernel stack at the point `enter_user_mode` saved its callee-saved registers,
/// pop them, and return into `run_process` with `status` in `rax`. The trap
/// stack the exception arrived on is simply abandoned — nothing on it is
/// needed again. Takes the BKL first, because the code it returns into is
/// kernel code and expects to hold it; the exception stub already `swapgs`'d,
/// so the per-CPU block is in place.
pub fn kill_current_from_fault(sig: u32) -> ! {
    crate::smp::bkl_enter();
    // Same group-death rule as `exit_current_from_signal`: a *thread* faulting
    // out of ring 3 by default action must not just kill itself, or the leader
    // runs on and nothing ever reaches the parent's `waitpid`.
    crate::signal::notify_group_of_thread_fatal(sig);
    unwind_to_kernel((-(i64::from(sig))) as u64)
}

/// Leave ring 3 from a timer tick because the thread's group is exiting
/// (`signal::TickOutcome::Exit`): the exit a syscall return would have taken
/// through `deliver_pending`'s group-status check, for a thread that makes
/// no syscalls. No group notification — the group is already on its way out,
/// which is why this thread is leaving. Takes the BKL like
/// [`kill_current_from_fault`], for the same reason: it returns into kernel
/// code (`run_process`/`run_thread`) that expects to hold it.
pub fn leave_current_from_tick(status: u64) -> ! {
    crate::smp::bkl_enter();
    unwind_to_kernel(status)
}

/// The common tail of [`kill_current_from_fault`] and
/// [`leave_current_from_tick`]: return `status` into whoever called
/// `enter_user`, as `.Lexit_to_kernel` would for a syscall that set `leave`.
fn unwind_to_kernel(status: u64) -> ! {
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
    // BKL-hold attribution (the AArch64 glue does this at every kernel entry;
    // amd64 never had it, so every `[BKL] stuck` line on the metal read
    // `tag=511` and named nothing — the profiler was on, the announcement
    // was not). Holding the BKL implies passing through here or through a
    // fault, so this one line is what makes `tag=` name the holder's syscall.
    akuma_bkl::sync::set_holder_tag(crate::smp::cpu_index_u32(), nr);
    // Per-slot, for `sched::dump_slot_table`: the pair `(thread state, this)`
    // says where a parked thread is parked, which the `[BKL]` tag above cannot
    // — that one is per *core* and names whoever last held the lock.
    crate::sched::note_syscall_entry(nr);
    // `ring0-preempt`: the syscall body runs with interrupts on, so the tick can switch
    // it out of a dropped-BKL window (`sched::ring0_preemptible`). `IA32_FMASK` cleared
    // `IF` on entry, which is what kept the stub's user-`rsp` window atomic; that window
    // is behind us. Masked again before the epilogue below.
    #[cfg(feature = "ring0-preempt")]
    akuma_cpu::interrupts::on();
    CALLS.fetch_add(1, Ordering::Relaxed);
    // Per-process syscall counters — the `[PSTATS]` machinery, aarch64 parity
    // (`akuma-kernel-glue` bumps the same counters through the `akuma-syscalls`
    // excursion hooks; this target's dispatcher is its own, so the bump lives
    // here). Counts are exact. Times are the **TSC**, microsecond resolution.
    //
    // This read the 10 ms LAPIC tick until 2026-09-17, and the note here said
    // that was the point: a syscall shorter than a tick folds to 0, the sweep
    // sorts by time, so what surfaces is where the wall clock went. That is
    // true of a *blocking* syscall and false of a frequent short one, and the
    // difference is not a rounding error — it inverts the answer. A 7 us
    // `mmap` is charged a full 10 ms whenever a tick happens to land inside
    // it, so its reported time is `calls x 10 ms x P(tick lands in it)`, which
    // is an unbiased estimator of total time with a standard error of one
    // whole tick per sample. At 59 043 `mmap` calls per `rustc` that read 8.27
    // s against a per-call cost measured (`userspace/memprobe/c/mmap_scale.c`)
    // at 7.7 us — 0.45 s — and there is no way to tell from the number alone
    // which of the two is wrong. `tsc_uptime_us` is the resolution this target
    // already has (`clock_gettime` moved onto it, see this file's
    // `fault_cost_tests`); the cost is one `rdtsc` plus a 128-bit mul/div at
    // each end, ~40 cycles, against a syscall floor three orders up.
    //
    // `None` while the TSC is uncalibrated (no PIT to measure it against): the
    // epilogue then adds nothing rather than a fabricated duration, the same
    // rule `tsc_uptime_us` itself follows.
    let stats_t0 = crate::lapic::tsc_uptime_us();
    if let Some(p) = current_process() {
        p.syscall_stats.inc(nr);
    }
    let trace = SYSCALL_TRACE.load(Ordering::Relaxed) && {
        let only = SYSCALL_TRACE_PID.load(Ordering::Relaxed);
        only == 0 || current_process().is_some_and(|p| u64::from(p.tgid) == only)
    };
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
        // The other two arguments and the time: `poll`'s timeout and how long a call really took are
        // what a "hangs after a UDP reply" trace needs (`UDP_REPLY_WAIT_TIMER_STORM.md`).
        serial::puts(" a2=0x");
        serial::put_hex(a2);
        serial::puts(" a3=0x");
        serial::put_hex(a3);
        serial::puts(" t=");
        serial::put_dec(crate::net::uptime_us());
        match nr {
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
            _ => {}
        }
        // The path arguments, from the table the `[sc!]` line uses too. The
        // first-arg-path syscalls all answer ENOENT for a path this kernel does
        // not serve, and without the path an ENOENT is a number with nothing
        // attached — which is exactly how long it took to see that `ps` was
        // failing on `stat("/proc/<pid>")` rather than on `getdents64`.
        let (p1, p2) = trace_paths(nr, a1, a2, a3, a4);
        if let Some(p) = p1 {
            serial::puts(" \"");
            crate::fd::trace_user_cstr(p);
            serial::puts("\"");
        }
        if let Some(p) = p2 {
            serial::puts(" -> \"");
            crate::fd::trace_user_cstr(p);
            serial::puts("\"");
        }
        serial::puts("\n");
    }
    // A sibling thread of a process that has called `exit_group` leaves here,
    // before the syscall runs: its address space is about to be freed and
    // anything it does with it from now on is a race with the reaper.
    let r = if crate::thread::should_leave_now() {
        mark_leave();
        crate::fd::errno::EINTR
    } else {
        syscall_dispatch(nr, a1, a2, a3, a4, a5, a6)
    };
    // `saturating_sub` rather than a plain one: a syscall that blocks can
    // resume on another core, and while KVM and this machine both present an
    // invariant, synchronised TSC, a kernel that assumes it cannot be read
    // backwards is one errata away from a wildly wrong duration.
    if let (Some(t0), Some(t1)) = (stats_t0, crate::lapic::tsc_uptime_us())
        && let Some(p) = current_process()
    {
        p.syscall_stats.add_time_us(nr, t1.saturating_sub(t0));
    }
    // Linux's error convention: `-4095..=-1`. Anything else is a value (an
    // `mmap` address is "negative" as an `i64` and is not a failure).
    // `EAGAIN`, `EINTR` and `ETIMEDOUT` are what a polling, waiting program
    // returns all day, and none of them is a missing file or a missing row.
    if (r >= 0u64.wrapping_sub(4095)
        && SYSCALL_TRACE_ERRORS.load(Ordering::Relaxed)
        && !matches!(r.wrapping_neg(), 4 | 11 | 110))
        || trace_nr_selected(nr)
    {
        trace_error(nr, a1, a2, a3, a4, r);
    }
    if trace {
        serial::puts("[sc] cpu=");
        serial::put_dec(crate::smp::cpu_index() as u64);
        serial::puts(" task=");
        serial::put_dec(crate::sched::current_task() as u64);
        serial::puts(" nr=");
        serial::put_dec(nr);
        serial::puts(" -> 0x");
        serial::put_hex(r);
        serial::puts(" t=");
        serial::put_dec(crate::net::uptime_us());
        serial::puts("\n");
    }
    // Masked from here to `sysretq`: the stub's exit reads this core's per-CPU block and
    // runs with the user `rsp` loaded, and the epilogue's `leave` decision is per core.
    #[cfg(feature = "ring0-preempt")]
    akuma_cpu::interrupts::off();
    let uctx = crate::smp::current_uctx();
    // **The signal epilogue.** Before the `leave` read below, because a fatal
    // default action sets that flag — and after the trace lines, so a syscall's
    // result is reported as the syscall computed it rather than as a signal
    // rewrote it.
    //
    // Costs one relaxed load of this thread's pending word when nothing is
    // pending, which is every syscall but a handful per boot.
    let r = crate::signal::deliver_pending(uctx, r);
    let leaving = crate::sched::read_current_uctx(|u| u.leave != 0).unwrap_or(false);
    if !leaving {
        crate::smp::bkl_leave();
    }
    r
}

/// Set the running task's `UserCtx::leave`: the syscall return path reads it and
/// `sysret`s nowhere, leaving ring 3 for good. A no-op with no `UserCtx` on this
/// core (the boot task), which is what the null check always meant.
fn mark_leave() {
    let _ = crate::sched::with_current_uctx(|u| u.leave = 1);
}

/// Leave ring 3 because a signal's default action says to, as if the program had
/// called `exit_group(-sig)`.
///
/// The negative status is the tree's encoding for "killed by signal": glue's
/// `encode_wait_status` turns it into `WIFSIGNALED`, and the AArch64 kernel's
/// own fatal-signal path spells it the same way
/// (`akuma_exceptions::fatal_signal_group_exit`).
///
/// Deliberately the same three lines as the [`Syscall::ExitGroup`] arm rather
/// than a call into `akuma_syscalls_glue::sys_exit_group`: this target's
/// teardown lives in [`run_process`]'s epilogue, and the `leave` flag is how a
/// task gets there. Glue's version stops short of the `SPAWN` row and parks the
/// thread in a `yield_now` loop, so a child killed through it would never report
/// a status to its parent's `sys_waitpid`.
///
/// Returns; the caller must not deliver any further signal, and the task leaves
/// ring 3 when the syscall return path reads `leave`.
pub fn exit_current_from_signal(sig: u32) {
    // A thread dying by signal takes its thread group with it (Linux
    // `do_group_exit`): pend the signal on the leader so *its* exit — the one
    // whose epilogue reaches the parent's `waitpid` — happens too. No-op for a
    // main thread, which is already that leader. See
    // `crate::signal::notify_group_of_thread_fatal`.
    crate::signal::notify_group_of_thread_fatal(sig);
    crate::thread::set_group_exiting(current_proc_slot());
    // The status travels in `%rax`: [`run_process`] reads `enter_user`'s
    // return value, which is why `signal::deliver_pending` returns `-(sig)`
    // rather than the interrupted syscall's result.
    mark_leave();
}

/// Leave ring 3 as `exit_group(code)` does: mark the group exiting and set
/// `leave`, which the syscall return path reads. The code itself is the
/// caller's return value, carried out in `%rax` to [`run_process`].
pub fn exit_current_group() {
    crate::thread::set_group_exiting(current_proc_slot());
    mark_leave();
}

/// `writev(fd, iov, iovcnt)` — `struct iovec` is `{ base: *const u8, len: usize }`,
/// 16 bytes. Forwards each segment through `sys_write`; a short or failing
/// segment ends the walk and the total so far (or the error, if nothing was
/// written) is returned, per POSIX.
fn sys_writev(fd: u64, iov: u64, cnt: u64) -> u64 {
    use crate::fd::errno;
    // A tap frame is one `writev`: splitting it per segment would send the header as its own frame.
    if crate::fd::is_dev_tun(fd) {
        return crate::kvm::tun::writev(iov, cnt as usize);
    }
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
    if crate::fd::is_dev_tun(fd) {
        return crate::kvm::tun::readv(iov, cnt as usize);
    }
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
    let bytes = info.to_bytes();
    if !crate::uaccess::write_bytes(out, &bytes) {
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

/// `memfd_create(name, flags)`: an unlinked file under `/tmp`, so `ftruncate` and a `MAP_SHARED` mmap
/// (twice, at adjacent addresses, as Firecracker's ring buffers do) behave as on a memfd. The name is
/// only a debugging label on Linux and is not read here. Costs ext2 blocks, not a tmpfs page.
fn sys_memfd_create(flags: u64) -> u64 {
    use crate::fd::errno;
    use core::sync::atomic::{AtomicU32, Ordering};
    const MFD_CLOEXEC: u64 = 1;
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let mut path = [0u8; 40];
    let prefix = b"/tmp/.memfd-";
    path[..prefix.len()].copy_from_slice(prefix);
    let mut n = prefix.len();
    let mut num = u64::from(SEQ.fetch_add(1, Ordering::Relaxed)) | u64::from(crate::smp::cpu_index() as u32) << 32;
    let mut digits = [0u8; 20];
    let mut d = digits.len();
    loop {
        d -= 1;
        digits[d] = b'0' + (num % 10) as u8;
        num /= 10;
        if num == 0 {
            break;
        }
    }
    path[n..n + digits.len() - d].copy_from_slice(&digits[d..]);
    n += digits.len() - d;
    let Ok(p) = core::str::from_utf8(&path[..n]) else { return errno::EINVAL };
    const O_RDWR: u32 = 2;
    const O_CREAT: u32 = 0o100;
    const O_EXCL: u32 = 0o200;
    const O_CLOEXEC: u32 = 0o200_0000;
    let oflags = O_RDWR | O_CREAT | O_EXCL | if flags & MFD_CLOEXEC != 0 { O_CLOEXEC } else { 0 };
    match akuma_syscalls_glue::fs::openat_path(-100, p, oflags, 0o600) {
        Ok(fd) => {
            let _ = akuma_vfs_glue::fs::remove_file(p);
            fd
        }
        Err(e) => e,
    }
}

/// The task slot `tid` names for the affinity calls: 0 is the caller; otherwise a process id
/// (what `taskset -p` and `ps` show), then, failing that, a thread id (what `gettid` returns).
fn affinity_slot(tid: u64) -> Option<usize> {
    if tid == 0 {
        return Some(akuma_exec::threading::current_thread_id());
    }
    let tid = u32::try_from(tid).ok()?;
    if let Some(slot) = akuma_exec::process::lookup_process_shared(tid).and_then(|p| p.thread_id) {
        return Some(slot);
    }
    let slot = tid as usize;
    (slot < akuma_exec::threading::MAX_THREADS && akuma_exec::process::find_pid_by_thread(slot).is_some()).then_some(slot)
}

/// `sched_setaffinity(tid, len, mask)`: the request is decoded and clipped by
/// `akuma_affinity`; `sched::set_affinity` stores it and moves the caller if needed.
fn sys_sched_setaffinity(tid: u64, len: u64, mask_ptr: u64) -> u64 {
    use crate::fd::errno;
    use akuma_affinity as affinity;
    const MAX: usize = 128;
    let len = len as usize;
    if len < 8 {
        return errno::EINVAL;
    }
    let mut buf = [0u8; MAX];
    let n = len.min(MAX);
    if !crate::uaccess::read_bytes(mask_ptr, &mut buf[..n]) {
        return errno::EFAULT;
    }
    // A caller's bytes past what we read are not inspected: a real cpu_set that large is all zero.
    let Some(slot) = affinity_slot(tid) else { return errno::ESRCH };
    let Ok(requested) = affinity::decode(&buf[..n]) else { return errno::EINVAL };
    let Ok(mask) = affinity::effective(requested, crate::smp::online_cpus()) else { return errno::EINVAL };
    if crate::sched::set_affinity(slot, mask) { 0 } else { errno::EAGAIN }
}

/// `sched_getaffinity(tid, len, mask)`: the stored mask, or every online core; returns the bytes
/// written (musl zeroes the rest from that count).
fn sys_sched_getaffinity(tid: u64, len: u64, mask_ptr: u64) -> u64 {
    use crate::fd::errno;
    let len = len as usize;
    if len < 8 {
        return errno::EINVAL;
    }
    let Some(slot) = affinity_slot(tid) else { return errno::ESRCH };
    let mask = akuma_affinity::report(crate::sched::affinity_of(slot), crate::smp::online_cpus());
    if !crate::uaccess::write_bytes(mask_ptr, &mask.to_le_bytes()) {
        return errno::EFAULT;
    }
    8
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
    match code {
        ARCH_SET_FS => {
            // A non-canonical base is refused, as Linux does (`EPERM`): the
            // `wrmsr` would `#GP` in ring 0, which here is a user-triggerable
            // kernel halt. A bad-but-canonical value can only fault the
            // program's own `%fs:` accesses.
            if msr::set_fs_base(addr).is_err() {
                return crate::fd::errno::EPERM;
            }
            // Also record it in this task's `UserCtx` so the scheduler can
            // restore it on the way back in — the MSR is CPU-global, and without
            // the per-task copy a forked child that `execve`s (and
            // re-`arch_prctl`s) leaves the parent running on the child's TLS
            // base. That crash (`cr2` a tiny offset off a garbage pointer) is
            // what made this field load-bearing.
            let _ = crate::sched::with_current_uctx(|u| u.fs_base = addr);
            0
        }
        ARCH_SET_GS => {
            // The program's GS lives in IA32_KERNEL_GS_BASE while the kernel
            // runs — IA32_GS_BASE is this core's per-CPU block right now, and
            // writing the program's value there would point every `gs:[..]` in
            // the kernel at user memory. Recorded per task for the same reason
            // as FS.
            if msr::set_user_gs_base(addr).is_err() {
                return crate::fd::errno::EPERM;
            }
            let _ = crate::sched::with_current_uctx(|u| u.gs_base = addr);
            0
        }
        ARCH_GET_FS => {
            if !crate::uaccess::write_val::<u64>(addr, msr::read(Msr::FsBase)) {
                return crate::fd::errno::EFAULT;
            }
            0
        }
        ARCH_GET_GS => {
            if !crate::uaccess::write_val::<u64>(addr, msr::read(Msr::KernelGsBase)) {
                return crate::fd::errno::EFAULT;
            }
            0
        }
        _ => crate::fd::errno::EINVAL,
    }
}

/// Does the running process have an I/O channel that is **not** the console's?
///
/// `Process::channel` is `Some` in exactly two cases here: a console-attached
/// process carries the serial line's channel, and a `sys_spawn` child carries
/// its own (inherited by everything it forks). Everything else — the boot task,
/// the self-test processes — is `None` and writes to the serial line, which is
/// what it always did.
///
/// Deliberately `Process::channel` and **not** `current_channel()`: that falls
/// back to the per-thread exit channel, which every registered process has, so
/// it would answer `true` for a self-test process and route its output into a
/// FIFO nobody drains. `AKUMA_AMD64_CONSOLE_PROCESSCHANNEL.md` §3 is the record
/// of that exact confusion costing a boot.
fn owns_non_console_channel() -> bool {
    akuma_exec::process::current_process_shared().is_some_and(|p| {
        p.channel
            .as_ref()
            .is_some_and(|ch| !crate::console::is_console_channel(ch))
    })
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
pub fn sys_write(fd: u64, buf: u64, len: u64) -> u64 {
    const EFAULT: u64 = (-14i64) as u64;

    // `/dev/wifi0`: command lines for the wifi driver (`crate::wifi`).
    if crate::fd::is_dev_wifi(fd) {
        return crate::wifi::write(buf, len as usize);
    }

    // `/dev/net/tun`: one frame behind its virtio-net header (`crate::kvm::tun`).
    if crate::fd::is_dev_tun(fd) {
        return crate::kvm::tun::write(buf, len as usize);
    }

    // `/dev/kvm0`: a VM spec, `stop` or `status` (`crate::kvm::ctl`). Refused whole.
    if crate::fd::is_dev_kvm(fd) {
        return crate::kvm::ctl::dev_write(buf, len as usize);
    }

    // `/dev/input/mouse0`: control lines (`natural`, `natural=0|1`) — the
    // scroll-direction toggle (`crate::touchpad::dev_write`).
    if crate::fd::is_dev_mouse(fd) {
        return crate::touchpad::dev_write(buf, len as usize);
    }

    // `/dev/power`: `dim=<percent>` lines for the power daemon's lever
    // (`crate::power`). Parsed whole before any line applies.
    if crate::fd::is_dev_power(fd) {
        return crate::power::dev_write(buf, len as usize);
    }

    // See the header: first, and by both spellings — **unless this process's
    // stdout belongs to a channel of its own**, which is what an `ssh` session's
    // child has had since its stdio stopped being pipes (2026-09-11). Its fd 1
    // and fd 2 are `Stdout`/`Stderr` descriptors like init's, so `console_end`
    // claims them by spelling, and answering them here would put every session's
    // output on the serial line instead of on the SSH stream. Glue's own
    // `Stdout`/`Stderr` arm writes `current_channel()` with the line
    // discipline's output translation and real backpressure, which is where they
    // belong; `crate::console::is_console_channel` is what tells the two apart,
    // because both report `is_terminal()` and nothing else distinguishes them.
    //
    // Asked **after** the cheap spelling test, so a write to a file or a socket
    // never pays for it.
    let console_write = crate::fd::console_end(fd) == Some(crate::fd::ConsoleEnd::Write)
        && !owns_non_console_channel();
    if !console_write {
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
    //
    // **Who is talking decides whether the TV sees it.** While a console shell
    // owns the framebuffer (`serial::set_fb_quiet`), pid 1 — `herd`, whose
    // `[herd] Started …` lines are supervisor chatter — is held to `dmesg`, and
    // everything else on the console channel (the shell and what it runs) is
    // shown. When no shell is running nothing is quiet and both paths are the
    // same, so `init=/bin/sh` still prints.
    let to_tv = current_pid() != 1;
    let mut chunk = [0u8; 256];
    let mut done = 0u64;
    while done < len {
        let n = ((len - done) as usize).min(chunk.len());
        if !crate::uaccess::read_bytes(buf + done, &mut chunk[..n]) {
            return if done == 0 { EFAULT } else { done };
        }
        for &byte in &chunk[..n] {
            if to_tv {
                serial::putb_tty(byte);
            } else {
                serial::putb(byte);
            }
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
    let star = akuma_gdt::star();
    // SAFETY: the four architectural MSRs of the fast-syscall path, written
    // before any `syscall` can be executed (userspace does not exist yet).
    unsafe {
        msr::write(Msr::Efer, msr::read(Msr::Efer) | EFER_SCE);
        msr::write(Msr::Star, star);
        msr::write(Msr::Lstar, syscall_entry as *const () as usize as u64);
        // Clear IF on entry, so a syscall handler never runs with interrupts on
        // while `rsp` still points into user memory. Also clear DF, so the
        // kernel's string operations start from a known direction — userspace
        // can set it and the ABI does not require it cleared on entry.
        // And AC (bit 18): with `CR4.SMAP` on, a program that sets AC and then
        // executes `syscall` would otherwise enter the kernel with SMAP
        // suspended — the one way userspace could grant itself the kernel's
        // access to user pages. Linux masks it for the same reason.
        //
        // **TF (8), IOPL (12:13) and NT (14) are masked for reasons of exactly
        // the same shape**, added 2026-09-12:
        //
        // * `TF` — `syscall` is not an interrupt gate and does not clear it, so
        //   a program that enters the kernel single-stepping makes the *kernel*
        //   single-step: a `#DB` on the first instruction after `syscall`, on a
        //   target that had no `#DB` handler at all. And a program can set it —
        //   `signal::sanitize_rflags` permits `TF` deliberately, as Linux does,
        //   because a debugger legitimately restores it. That pair (userspace
        //   may set it, the entry does not clear it) is a ring-3 kill switch
        //   for the whole machine.
        // * `IOPL`/`NT` — `sysret` loads `RFLAGS` from `%r11` and its mask
        //   covers both, so they can be non-zero on entry; `NT` set turns the
        //   kernel's next `iret` into a task switch.
        //
        // Linux masks all of these and more in `MSR_SYSCALL_MASK`; this is the
        // same list minus the arithmetic flags, which are harmless and which
        // the ABI lets a syscall clobber anyway.
        const FMASK_IF: u64 = 1 << 9;
        const FMASK_DF: u64 = 1 << 10;
        const FMASK_AC: u64 = 1 << 18;
        const FMASK_TF: u64 = 1 << 8;
        const FMASK_IOPL: u64 = 3 << 12;
        const FMASK_NT: u64 = 1 << 14;
        msr::write(
            Msr::Fmask,
            FMASK_IF | FMASK_DF | FMASK_AC | FMASK_TF | FMASK_IOPL | FMASK_NT,
        );
    }
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
pub struct Image {
    /// The address space the loader built, with the program in it.
    ///
    /// A plain [`UserAddressSpace`], not a [`ProcAddressSpace`]: nothing can
    /// reach this space concurrently until it is registered, and the lock and
    /// the lock-free root mirror are exactly what registration adds.
    pub space: UserAddressSpace,
    /// Where ring 3 starts executing.
    pub entry: u64,
    /// Initial `rsp`.
    pub stack: u64,
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
    pub regions: Vec<MmapRegion>,
}

impl Image {


    /// As [`Self::from_elf`], with the argv the program sees on its initial
    /// stack and no environment. The self-tests pass a single element because
    /// `hello.rs` only checks `argv[0]`; the syscall paths carry an environment
    /// and go through [`Self::from_elf_argv_envp`] directly.
    pub fn from_elf_argv(
        image: &[u8],
        argv: &[&[u8]],
    ) -> Result<(Self, loader::LoadedImage), &'static str> {
        Self::from_elf_argv_envp(image, argv, &[])
    }

    /// As [`Self::from_elf_argv`], plus the environment. Both `execve` and
    /// `sys_spawn` hand the new image the caller's `envp`; `from_elf_argv` (the
    /// self-tests) passes none, and a program with an empty environment falls
    /// back to its own default `PATH`.
    fn from_elf_argv_envp(
        image: &[u8],
        argv: &[&[u8]],
        envp: &[&[u8]],
    ) -> Result<(Self, loader::LoadedImage), &'static str> {
        Self::with_stack(loader::load(image)?, argv, envp)
    }

    /// As [`Self::from_elf_argv_envp`], loading the image off the file at
    /// `path` (symlink-resolved) a window at a time rather than from a copy in
    /// the heap — what `execve` and `sys_spawn` use (`loader::load_path`).
    fn from_elf_path_argv_envp(
        path: &str,
        argv: &[&[u8]],
        envp: &[&[u8]],
    ) -> Result<(Self, loader::LoadedImage), &'static str> {
        Self::with_stack(loader::load_path(path)?, argv, envp)
    }

    /// Give a loaded image its initial stack.
    fn with_stack(
        (mut space, img): (UserAddressSpace, loader::LoadedImage),
        argv: &[&[u8]],
        envp: &[&[u8]],
    ) -> Result<(Self, loader::LoadedImage), &'static str> {
        // `space` drops here on a stack failure, which frees the frames the
        // loader placed along with the page tables it built. That is exactly
        // the `free_all_frames` + `space.free()` pair this arm used to run by
        // hand, and it can no longer be forgotten on a new arm.
        let stack = loader::build_stack(&mut space, ELF_STACK_TOP, ELF_STACK_PAGES, argv, envp, &img)?;

        let entry = img.entry;
        let regions = alloc::vec![crate::mm::lazy_stack_region(ELF_STACK_TOP, ELF_STACK_PAGES, ELF_STACK_MAX_PAGES)];
        Ok((Self { space, entry, stack, regions }, img))
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

}

/// Share the parent's user memory into `child_space`, copy-on-write, and fill
/// `child_regions` with the extents the child inherits.
///
/// **One walk of the parent's tables, with the parent's own PTE edited in place
/// where it has to be demoted** — see the comments inside for why each of the
/// three leaf cases is what it is.
///
/// Extracted from [`Image::fork_of`] on 2026-09-10 with C1 slice 4, because it
/// has two callers now: that one, and the `ExecRuntime::fork_share_memory`
/// registration through which the **shared** `akuma_exec::process::fork_process`
/// builds a child on this target. One walk, two spellings, so the tree's `fork`
/// and this target's own cannot describe a child's memory differently.
///
/// Returns `false` if any page failed to map; the caller owns what to do about
/// it — `fork_of` drops the whole address space, the hook reports an error and
/// lets `fork_process`'s own unwinding release the child.
/// **`ExecRuntime::fork_share_memory` on this target** — build a `fork` child's
/// address space from its parent's, for the shared
/// `akuma_exec::process::fork_process`.
///
/// The shared implementation of this step
/// (`akuma_exec::process::fork_share_parent_memory`) cannot run here, and the
/// reason is the whole of C1 slice 4: it reaches every page through
/// `akuma-mmu`'s raw-root walker family, which reads **AArch64 descriptor
/// bits** and is not `#[cfg]`-gated. Handed an x86 PML4 it would descend by
/// ARM rules — ARM's `VALID` is x86's `Present`, ARM's `TABLE` is x86's `R/W` —
/// and build a garbage address space without erroring.
///
/// So this target registers its own, over the leaf iterator that *does* have
/// both arms. It is [`share_parent_memory_into`], i.e. the same walk
/// `Image::fork_of` has always used, pointed at the child `fork_process`
/// already built rather than at a fresh one.
///
/// The child is unpublished and in the caller's exclusive hands, so
/// `get_mut()` on its address space is sound and no lock is taken on it; the
/// parent's is taken inside, for the whole walk.
///
/// **The `ProcessInfo` page is not this function's problem.** `fork_process`
/// maps one into the child before calling here and re-maps it *after*, because
/// this walk covers `PROCESS_INFO_ADDR` and would otherwise leave the child
/// sharing the parent's copy — which is the same ordering the AArch64 side
/// carries, for the same reason.
pub fn fork_share_memory(
    parent: &akuma_exec::process::Process,
    child: &mut akuma_exec::process::Process,
) -> Result<(), &'static str> {
    let mut regions: Vec<MmapRegion> = Vec::new();
    let ok = share_parent_memory_into(parent, child.address_space.get_mut(), &mut regions);
    if !ok {
        serial::puts("  [fork] share pass failed; pmm free=");
        serial::put_dec(akuma_pmm::free_count() as u64);
        serial::puts("\n");
        return Err("fork: CoW share pass failed");
    }
    *child.mmap_regions.lock() = regions;
    Ok(())
}

fn share_parent_memory_into(
    parent: &akuma_exec::process::Process,
    child_space: &mut UserAddressSpace,
    child_regions: &mut Vec<MmapRegion>,
) -> bool {
    let mut ok = true;

    // The child's region list, and — separately — the VA ranges that must be
    // shared **by identity** rather than copy-on-write. Both are read out of
    // the parent's list in one hold, before the page walk below, because the
    // walk maps pages and must not run under the region lock.
    // **The leader's region list, not the forking thread's** (2026-10-03). A
    // `CLONE_THREAD` thread's own `mmap_regions` is empty by construction —
    // the address space's regions live on the thread-group leader, which is
    // what `current_mm_process` resolves for every other memory syscall. A
    // fork from a worker thread (Python's `subprocess`, tokio's
    // `spawn_blocking` running `Command`) read the empty list, so the child
    // inherited **no regions** (its lazy pages faulted as unmapped) and,
    // worse, no shared ranges: every `MAP_SHARED` page was CoW-demoted in the
    // parent, whose next write then took a private copy and silently left the
    // sharing. With SQLite's `-shm` in the shared writable page table, that
    // private copy was then written back over every other process's live
    // index — a multi-process WAL database corrupted by any tool call
    // (`docs/archive/AKUMA_AMD64_SHARED_WRITABLE_PAGES.md`).
    let owner = akuma_exec::process::lookup_process_shared(parent.tgid).unwrap_or(parent);
    let (inherited, shared_ranges) = {
        let _irq = akuma_primitives::irq::IrqGuard::new();
        let parent_regions = owner.mmap_regions.lock();
        // Writable `MAP_SHARED` file mappings too (2026-10-03): their pages
        // are one frame per file page for every mapper, so a child shares the
        // parent's frames exactly as it does a shared anonymous mapping's.
        // Copy-on-write here was the old answer, and it split the child off
        // the file the moment either side wrote.
        let shared: Vec<(usize, usize)> = parent_regions
            .iter()
            .filter(|r| r.shared_anon || r.shared_write.is_some())
            .map(|r| (r.start_va, r.start_va + r.len_bytes()))
            .collect();
        // The child maps every page of every parent region — read-only and
        // CoW-shared by the pass below — but *owns* none of them, which is
        // exactly the shape `inherit_mmap_regions_for_cow_child` produces.
        // Carrying the **extent** across is the part that matters and the
        // part that has been dropped before: a grandchild whose parent's
        // regions read as zero-length shares nothing and faults on its first
        // touch (`docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md`).
        let mut inherited = akuma_mmap::inherit_mmap_regions_for_cow_child(&parent_regions);
        // The shared crate drops `shared_write` on inheritance, because a
        // CoW child owns no frames to write back — true on AArch64, whose
        // fork copies. Here the child maps the **same** frames as the parent
        // (the share pass below), so it is a mapper like any other: it must
        // write back on `munmap`/exit and release its table references, and
        // without the record it would do neither. Index-aligned, because the
        // inheritance maps over the parent's list in order.
        for (child, parent) in inherited.iter_mut().zip(parent_regions.iter()) {
            if parent.shared_write.is_some() && child.start_va == parent.start_va {
                child.shared_write.clone_from(&parent.shared_write);
            }
        }
        (inherited, shared)
    };
    *child_regions = inherited;

    // One walk of the parent's tables, with the parent's own PTE edited in
    // place where it has to be demoted.
    //
    // `rewrite_leaves_in_range` replaces `for_each_user_leaf` + a second
    // `map_page_in` per demoted page: the walk already has the leaf slot in
    // hand, so a `Reprotect` is one store rather than a fresh four-level
    // descent. The child's pages are mapped inside the closure into
    // `child_space`, a **different** address space, so nothing here edits the
    // table the walk is standing on.
    //
    // The parent's hold is taken for the whole walk. That is the longest
    // `ProcAddressSpace` hold on this target and it is bounded by the
    // parent's residency; it has to be one hold, because a demote that
    // published halfway would leave the parent writable on pages the child
    // already shares.
    // The **owner's** lock, not `parent.address_space` — `bkl_guard.rs`
    // constraint 1, which this hook's first cut ignored. `fork_process` runs
    // step 4 with the BKL dropped (`no-bkl-process`), so this hold is what
    // serializes the walk against the CoW fault handler, which takes the same
    // lock for its whole break (`cow_write_fault` in `idt.rs`) and against
    // `madvise`/`munmap`, which take it through `with_current_address_space`.
    // All three must name the *same* lock object: the thread-group leader's,
    // whose `ProcAddressSpace` owns the live L0. `parent` is the forking
    // thread's `Process` — for a worker-thread fork (`CLONE_THREAD` sibling)
    // that is a shared-L0 view under a **fresh** lock nothing else in the
    // system takes, and the hold above would exclude nothing. The owner is the
    // leader, and `tgid` is the leader's pid on both kernels.
    let mut parent_as = owner.address_space.lock();
    // The child inherits the parent's file mappings — including the pages of
    // them nobody has faulted yet — so it must inherit their claim on the
    // files. Without this the child's demand-paged mappings are kept alive only
    // by the parent's pins, and a parent that execs or exits first leaves the
    // child faulting against an inode the filesystem is free to reissue.
    // Beside the region inheritance above in intent; here in position because
    // this is where the parent's address space is in hand.
    child_space.inherit_mapped_inode_pins(&parent_as);
    parent_as.rewrite_leaves_in_range(0, akuma_mmu::USER_HALF_END, |_ledger, leaf| {
        if !ok {
            return LeafAction::Keep;
        }
        let (va, pa) = (leaf.va, leaf.pa);
        let frame = PhysFrame::new(pa);

        // The per-leaf decision is `akuma_user_tree::fork_leaf` — one table, host-tested against
        // `PteLayout::plan_clone` (`fork_leaf_agrees_with_plan_clone`), so this decoded-leaf walk and a
        // raw-entry walker cannot disagree. The arms below are its four classes, in its order.
        let in_shared = shared_ranges.iter().any(|(start, end)| va >= *start && va < *end);
        let class = akuma_user_tree::fork_leaf(
            if leaf.attr == akuma_mmu::MemAttr::WriteBack {
                akuma_user_tree::Backing::Ram
            } else {
                akuma_user_tree::Backing::Device
            },
            if in_shared { akuma_user_tree::Span::Shared } else { akuma_user_tree::Span::Private },
            leaf.prot.write,
            leaf.cow,
        );

        // A device page (`/dev/fb0`'s write-combining framebuffer) is not PMM
        // RAM: no share count, no ledger entry, no copy-on-write — the child
        // simply maps the same device memory the same way. Counting it would
        // hand the framebuffer to the page allocator when the child exits.
        if class == akuma_user_tree::ForkLeaf::Device {
            if !child_space.map_device_page(va, pa, leaf.prot, leaf.attr) {
                ok = false;
            }
            return LeafAction::Keep;
        }

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
        if class == akuma_user_tree::ForkLeaf::ShareWritable {
            let shared_rw = PteProt { write: true, ..leaf.prot };
            // One share-count reference per **frame**, not per VA: a frame the
            // parent maps at two VAs (one file mapped twice) is one reference
            // the child's teardown gives back once. `adopt_user_frame` is the
            // ledger's rule for exactly that.
            let _ = child_space.adopt_user_frame(frame, false);
            if !child_space.map_page_pte(va, pa, shared_rw, false) {
                if child_space.remove_user_frame(frame) && akuma_pmm::cow_ref_dec(frame.addr) {
                    akuma_pmm::free_page(frame.addr, 0);
                }
                ok = false;
            }
            return LeafAction::Keep;
        }

        // A page that is already read-only and *not* CoW stays exactly as it
        // is in both spaces — `.rodata`, an `mprotect(PROT_READ)` region.
        // Marking it would turn a legitimate `SIGSEGV` into a silent write.
        if class == akuma_user_tree::ForkLeaf::DemoteBoth {
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
            child_space.track_user_frame(frame);
            if !child_space.map_page_pte(va, pa, demoted, true) {
                if child_space.remove_user_frame(frame) && akuma_pmm::cow_ref_dec(frame.addr) {
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
            child_space.track_user_frame(frame);
            if !child_space.map_page_pte(va, pa, leaf.prot, leaf.cow) {
                if child_space.remove_user_frame(frame) && akuma_pmm::cow_ref_dec(frame.addr) {
                    akuma_pmm::free_page(frame.addr, 0);
                }
                ok = false;
            }
            LeafAction::Keep
        }
    });
    drop(parent_as);

    ok
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
/// (`ArchHooks::current_slot`), and that is the same key
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
#[must_use]
pub fn current_process() -> Option<&'static akuma_exec::process::Process> {
    akuma_exec::process::current_thread_own_process().map(|(_pid, p)| p)
}

/// Run `f` with the region list **and** the process's `mmap` placement cursor.
///
/// The sibling of [`with_current_regions`], and it exists because the cursor and
/// the list have to be read and updated under the same hold: two cores placing
/// at once must not both read the same cursor, find the same gap and record two
/// regions at one address. `mm::find_free_va_from` reads it, the placement
/// writes it, and the region insert that makes the choice real all happen inside
/// one closure.
///
/// The cursor is `ProcessMemory::next_mmap` — the AArch64 kernel's own
/// per-process mmap cursor, which this target had simply never used. Reusing it
/// rather than adding a field is what keeps `fork` correct for free: that kernel
/// already copies it to the child (`process::mod`'s two `next_mmap.store` lines),
/// and a child that inherited an empty cursor would re-walk its parent's whole
/// address space on its first `mmap`.
pub fn with_current_regions_and_cursor<R>(
    f: impl FnOnce(&mut Vec<MmapRegion>, &core::sync::atomic::AtomicUsize) -> R,
) -> Option<R> {
    let p = current_mm_process()?;
    let _irq = akuma_primitives::irq::IrqGuard::new();
    let cursor = &p.memory.next_mmap;
    let mut regions = p.mmap_regions.lock();
    Some(f(&mut regions, cursor))
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
    let p = current_mm_process()?;
    let _irq = akuma_primitives::irq::IrqGuard::new();
    Some(f(&mut p.mmap_regions.lock()))
}

/// The process whose **memory** the running task uses: itself, or its
/// thread-group leader if it is a `CLONE_THREAD` child.
///
/// One address space per thread group is what `CLONE_VM` means, and on this
/// target the demand-paging state that describes that address space is the
/// `Process`'s own `mmap_regions` — so a thread asking "is this VA mine?"
/// through its *own* `Process` asks a list that is empty by construction
/// (`Process::inherit_from` gives every child a fresh one).
///
/// **This is not a tidy-up; it is the bug the `clone` fold found.** Before the
/// fold a thread had no `Process` at all and `THREAD_PID_MAP` published it
/// under its *leader's* pid, so `current_process()` already answered with the
/// leader — [`current_process`]'s own doc said so ("`own`, not `tgid`"). The
/// shared `spawn_child_thread_and_publish` publishes a thread under its own
/// pid, which is right for identity (`gettid`, signals, the exit channel) and
/// wrong for memory. The first `clone` child after the fold faulted on the
/// **first push to its own stack**: `.bss` is demand-paged here, the fault
/// handler asked this thread's empty region list whether `cr2` was mapped, got
/// "no", and killed the process. `#PF ... rip=0x400023 cr2=rsp` — the child's
/// very first instruction after the `clone` returned.
///
/// Splitting the question in two is what Linux does: identity is per-thread,
/// the `mm` is per-thread-group. `Process::tgid` is the leader's pid on both
/// kernels (`inherit_from` copies the parent's for a `CLONE_THREAD` child), so
/// the leader is one lookup away.
///
/// This is `akuma-exec`'s own `current_thread_tgid_process()`, which is the
/// **group** half of a pair the other kernel has had all along —
/// `current_thread_own_process()` being the per-thread half. Both resolve
/// through the cached `THREAD_IDENTITY` table, so asking the right one costs
/// the same as asking the wrong one. amd64 reached for the own-half everywhere
/// only because, before the fold, a thread had no `Process` of its own and the
/// two answers coincided.
fn current_mm_process() -> Option<&'static akuma_exec::process::Process> {
    akuma_exec::process::current_thread_tgid_process().map(|(_tgid, p)| p)
}

/// The pid of [`current_mm_process`] — the thread-group leader whose region
/// list every memory syscall edits. Trace-only.
pub fn current_mm_pid() -> u32 {
    current_mm_process().map_or(0, |p| p.pid)
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
/// this target — `kernel_bkl_smp` is a required feature of it since the
/// unblock (`docs/archive/AKUMA_AMD64_SMP_SHARED_UNBLOCK.md`) and `akuma-cpu`'s
/// `daif` has real x86 arms.
///
/// Lock order is **regions → address space**: `fault_in` and `dontneed_range`
/// take the region list first and reach the tables inside it. Nothing takes them
/// the other way round, and nothing may start.
pub fn with_current_address_space<R>(f: impl FnOnce(&mut akuma_mmu::UserAddressSpace) -> R) -> Option<R> {
    // The **leader's**, for the reason `current_mm_process` gives: one address
    // space per thread group. A `CLONE_THREAD` child's own `address_space` is a
    // `new_shared` *view* of the same page tables — the walk would work — but
    // its frame ledger is separate, so a page mapped through the view is
    // charged to a `Process` that will be reaped before the leader is, and
    // freed out from under it. Ask the owner.
    let p = current_mm_process()?;
    Some(f(&mut p.address_space.lock()))
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


/// Read and clear this task's [`UserCtx::forked`] flag — one-shot, and
/// consumed by [`run_process`] only.
///
/// It was two flags. `exec_pending` is still one-shot but is consumed in the
/// syscall return path's `.Lexec_return`, not here: since `execve` returns into
/// the new image rather than leaving ring 3, nothing in Rust is running at the
/// moment it is spent.
fn take_uctx_flag(read: impl Fn(&mut UserCtx) -> &mut u64) -> bool {
    // Under the BKL; only this task reads or clears these two fields.
    crate::sched::with_current_uctx(|u| {
        let f = read(u);
        let was = *f != 0;
        *f = 0;
        was
    })
    .unwrap_or(false)
}

/// **`ExecRuntime::enter_user` on this target**: enter ring 3 with a process's
/// first context, and never come back.
///
/// This is the amd64 half of the ring-3 entry seam
/// (`proposals/NEXT_AGENT_AMD64_RING3_ENTRY_SEAM.md` §4, option 1). The shared
/// `akuma_exec::process::Process::run` ends in `(runtime().enter_user)(&ctx)`;
/// on AArch64 that is an `eret` and the story ends there, because a process's
/// exit is handled from inside the syscall path. Here `sysret` **returns**, so
/// everything after the entry — the `execve` loop and the whole teardown — has
/// to live on this side of the hook. `-> !` is what says so.
///
/// The slot comes from the running task's own `UserCtx`, seeded by
/// [`crate::sched::seed_proc_slot`] before the task was published, for the same
/// reason [`proc_entry`] reads it there: shared code has no process-slot
/// concept to hand one down.
pub fn enter_ring3(first: &UserContext) -> ! {
    let idx = current_proc_slot();
    if idx >= PROC_SLOTS {
        // A task published without being seeded. `proc_entry` catches this
        // first for the tasks it starts; the check is repeated here because
        // this function is a registered hook and its other caller (slice 3's
        // shared `entry_point_trampoline`) does not go through `proc_entry`.
        serial::puts("  [proc] ring-3 entry with no slot\n");
        crate::sched::finish();
    }
    // **A thread is not a process, and its exit is not a process's exit.**
    //
    // Both arrive here since the `clone` fold: `entry_point_trampoline` ends in
    // `Process::run` for every child the shared spawn path builds, and a
    // `CLONE_THREAD` child is one of those. What differs is entirely below the
    // ring-3 entry — `run_process`'s teardown closes the fd table, drains the
    // thread group, publishes an exit status a parent's `wait4` will believe
    // and retires the process. Running that when *one thread* returns would
    // report the whole process dead while its siblings are still executing.
    //
    // `crate::thread::run_thread` is the other teardown: zero
    // `clear_child_tid` and wake one futex waiter on it (which is
    // `pthread_join`), purge the futex table of this task, drop its identity
    // row. The distinction is `UserCtx::thread_slot`, seeded by
    // `thread::bind_clone_child` before the task was published — the same field
    // `thread_entry` has always read, reached one function earlier.
    let tslot = crate::thread::current_thread_slot();
    if tslot != crate::thread::NO_THREAD {
        crate::thread::run_thread(tslot, first)
    }
    run_process(idx, first)
}

/// Enter ring 3 for process slot `idx` at `first`, then mark the task finished.
///
/// The scheduler has already installed this task's address space by the time
/// this runs — `spawn_in_space_unpublished` recorded the root, and `yield_now`
/// writes `CR3` before switching stacks.
///
/// `first` is the context `Process::run` read out of the registered process
/// under the `image` lock, which is where the entry point and stack have come
/// from since 5b slice 4 — they were module constants while every process was
/// the same hand-assembled blob at the same address. The loop re-reads them
/// with [`current_entry_stack`] on the way round, because that is what an
/// `execve` rewrites; the first pass uses what it was handed, which is the
/// same two scalars read microseconds earlier under the same lock (nothing can
/// rewrite them in between: this task has not executed a user instruction yet,
/// so it cannot have `execve`d, and no other task writes another process's
/// image).
fn run_process(idx: usize, first: &UserContext) -> ! {
    // A `fork` child's first entry re-enters ring 3 at the parent's post-`fork`
    // instruction with the parent's full register set. Read once, and spent by
    // that entry.
    let forked_child = take_uctx_flag(|u| &mut u.forked);
    // **One entry, one exit.**
    //
    // `execve` used to bring this task back here: it set `leave`, the entry
    // returned, and a loop re-read the new image's entry point out of the
    // registered process and went in again. It does not any more —
    // `sys_execve` writes `UserCtx::user_rip`/`user_rsp` and sets
    // `exec_pending`, and the syscall return path's `.Lexec_return` `sysret`s
    // straight into the new image. That is how Linux does it, and it is what
    // makes this a process lifecycle shaped like the AArch64 kernel's
    // (`Process::run` is `-> !` there; the exit leaves from inside the syscall
    // path) rather than a loop with a teardown hanging off the end.
    //
    // What the loop cost was never the four lines. It was that one task could
    // be below ring 3 more than once for one process, so everything after the
    // entry — the teardown, the exit status, the reap ordering — had to be
    // written as "the last time round". There is no last time round now.
    //
    // SAFETY: both are addresses the loader (or `Image::new`) mapped
    // user-accessible in the address space the scheduler installed for this
    // task, and every program this kernel runs ends in exit_group.
    let status = enter_user(first.pc, first.sp, forked_child);
    // **This task slot is the process's tid, and it stays the zombie's until the
    // reap** (`hold_exited_thread_slot`; `unregister_process` releases it).
    // Reissued at `sched::finish` below, as it used to be, the tid named the
    // zombie and its successor at once, and a `kill` of the zombie could reach
    // the successor (2026-10-16, ryzen). First thing out of ring 3, while the
    // slot is still running and before `spawn_record_exit` lets the parent reap
    // — a hold set after a reap would never be released.
    if let Some(p) = current_process() {
        akuma_exec::process::hold_exited_thread_slot(p.pid, crate::sched::current_task());
    }
    // Real Linux closes every fd a process still holds at exit. Since step 4b
    // the registered table is the only descriptor authority and every entry in
    // it owns a real pipe/socket reference, so the sweep is the table's own
    // `close_all()` — the same walk its `Drop` would run later — captured
    // **before** `thread::drain`, which may retire the identity the lookup
    // below resolves through. Done before `spawn_record_exit` so a parent's
    // `waitpid` never observes the child as reaped while its fds are still
    // charged against the shared table.
    let exit_fds = current_process().map(|p| p.fds.clone());
    // Every pid in this thread group, captured **before** `thread::drain` can
    // retire the threads' rows. Each thread has a pid of its own here, and a
    // child records the *spawning thread's* pid as its parent — so a
    // multi-threaded process's children are mostly its workers' children
    // (kot's tokio workers running `sh -c`), and `spawn_record_exit`'s
    // `reparent_children_to(dying, 1)` covers only the leader's. Without this
    // every such child outlived its process as an orphan nobody could `wait`
    // for: a zombie with `PPid` of a dead thread for the life of the boot
    // (2026-09-24, `docs/archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md` §6
    // item 6). Glue's `sys_exit_group` walks the group the same way.
    let group_members: Vec<u32> = current_process().map_or_else(Vec::new, |p| {
        let tgid = p.tgid;
        akuma_exec::process::collect_pids(|q| q.tgid == tgid)
    });
    // Every thread of this process, gone, before anything downstream can reap.
    // `sys_waitpid` retires the `Process` — and with it the page tables, which
    // the reclaim then frees — once this task is `Finished`, so a sibling still
    // running in that space would be walking freed page tables. This is the
    // only place that ordering can be enforced: the reaper is another process
    // and has no idea threads exist.
    crate::thread::drain(idx);
    // Writable `MAP_SHARED` file mappings get `munmap`'s treatment before the
    // address space is dropped: written back, and released from the shared
    // writable page table. After the drain, so no sibling can fault a page
    // back in behind it. See `mm::release_shared_write_mappings`.
    crate::mm::release_shared_write_mappings();
    // A process that owned `/dev/fb0` hands the screen back as it exits.
    if let Some(p) = current_process() {
        crate::fbdev::release(p.tgid);
    }
    if let Some(fds) = exit_fds {
        fds.close_all();
    }
    if idx >= SPAWN_SLOT_BASE {
        spawn_record_exit(idx, status as i32);
    }
    for member in group_members {
        let _ = akuma_exec::process::reparent_children_to(member, 1);
    }
    // Drop this task's entry in the **per-thread** channel registry, the way
    // the AArch64 exit epilogue does (`process/mod.rs`'s `remove_channel(tid)`
    // in `return_to_kernel`). `CHILD_CHANNELS` still holds the same `Arc` —
    // that one is the parent's to reap through `wait4` — so what this releases
    // is the registry slot signal delivery and `should_interrupt_blocking_syscall`
    // resolve through, which nothing should reach for a task that has left ring
    // 3 for good.
    //
    // **After `spawn_record_exit`, and the order is load-bearing.** That call
    // publishes the exit through `publish_child_exit`, which raises SIGCHLD
    // only when *it* is the call that marked the channel exited. Setting the
    // exit code here first — which is what the AArch64 site does, on the same
    // `Arc`, as a deliberate redundant re-set — would make the publish a no-op
    // and the parent would never get the signal. Unconditional, unlike the
    // publish: a boot self-test process is registered (and therefore has a
    // channel) but owns no `SPAWN` row.
    let ch = akuma_exec::process::channel::remove_channel(crate::sched::current_task());
    if let Some(ch) = ch {
        ch.set_exited(status as i32);
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
///
/// # It ends in the shared `Process::run`, since the ring-3 entry seam
///
/// It used to call [`run_process`] directly. It goes through
/// `akuma_exec::process::Process::run` — the same function AArch64's
/// `entry_point_trampoline` ends in — which activates the address space,
/// marks the process `Running` and calls back into [`enter_ring3`] through
/// `ExecRuntime::enter_user`. Three things change on this target, and all
/// three are the shared code closing a gap rather than a new behaviour:
///
/// * **The thread/process ownership gate.** `run()` refuses to enter ring 3
///   when `THREAD_PID_MAP` says this task belongs to a *different* pid —
///   proof it would otherwise install a foreign address space and jump to
///   this process's entry point inside it. This target had no such check.
/// * **`ProcessState::Running`.** Every process registered here was left
///   `Ready` for its whole life, because `prepare_for_execution` is not on
///   this target's path; `/proc/<pid>/stat` said `R` only by luck of the
///   default.
/// * **One extra `mov cr3`,** and this is the cost rather than a gain:
///   `run()` calls `address_space.activate()`, and the scheduler has already
///   installed exactly that root from the task slot's own `space_root`
///   (`spawn_in_space_unpublished` recorded it from the same `Image`). It is
///   one TLB flush per process launch, not per entry — the `execve` loop is
///   inside [`run_process`], below the hook — and it is stated here rather
///   than removed, because "the scheduler already did it" is a property of
///   this target's spawn path and not of `run()`.
extern "C" fn proc_entry() -> ! {
    let slot = current_proc_slot();
    if slot >= PROC_SLOTS {
        // Reached only if a task was published without being seeded. Say so:
        // `run_process` would name no process at all, and a panic here reads
        // as a scheduler fault rather than a missing seed.
        serial::puts("  [proc] entry with no slot\n");
        crate::sched::finish();
    }
    // The teardown below the entry reports an exit — `spawn_record_exit`
    // publishes a status a parent's `wait4` will believe — so a task that
    // never ran a program must not reach it. Unreachable by construction since
    // 5b slice 4 (every process task is registered before it is published) and
    // checked rather than assumed.
    let Some(proc) = current_process() else {
        serial::puts("  [proc] slot ");
        serial::put_dec(slot as u64);
        serial::puts(" has no registered process; not entering ring 3\n");
        crate::sched::finish();
    };
    proc.run()
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
pub fn spawn_process_task(proc_slot: usize, root: u64) -> Option<usize> {
    let task_slot = crate::sched::spawn_in_space_unpublished(proc_entry, root)?;
    crate::sched::seed_proc_slot(task_slot, proc_slot);
    // **A new process in this slot starts with a clean group.** Both flags are
    // indexed by process slot, and slots are recycled: `sys_spawn` takes the
    // lowest free SPAWN row, so the slot a dead process just vacated is the
    // very next one handed out. Until 2026-09-24 only the `fork` path cleared
    // them, and a spawn inherited whatever the previous occupant left:
    //
    // - `GROUP_EXIT_STATUS` — a multithreaded process whose worker died by a
    //   default-action signal (`notify_group_of_thread_fatal`) stamps
    //   `-(sig)` here. `deliver_pending` checks it before anything else, so
    //   every later spawn into the slot exited at its first syscall return as
    //   that signal. The captured case: `kot` (tokio, many workers) took a
    //   `SIGTERM` from herd or a `kill <pid>`, a worker died of it, and from
    //   then on **every** exec on the box returned 241 (`-15 & 0xff`) with no
    //   output until a power cycle — `sshd`'s shells, herd's restarts of kot
    //   itself.
    // - `GROUP_EXIT` — left set by any multithreaded `exit_group`. Harmless to
    //   a single-threaded successor (the main thread never tests it), but
    //   every thread a threaded successor creates would leave ring 3 at its
    //   first syscall.
    //
    // Cleared here rather than at each caller because this is the one claim
    // every non-`fork` process goes through (`sys_spawn`, init, the self-tests).
    // Regression: `spawn_stale_group_state_test`.
    crate::thread::clear_group_exiting(proc_slot);
    crate::thread::clear_group_exit_status(proc_slot);
    Some(task_slot)
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
// reads the stdout pipe. `waitpid` reports the exit status; the client's
// keystrokes arrive through the shared `write_to_process_stdin`, which finds
// the child's own fd 0 and writes the pipe behind it — `sshd` opens the real
// procfs `/proc/<pid>/fd/0` for that, the same call it makes on the other
// kernel. This file answered that path itself until 2026-09-11.
//
// No `fork`, no per-process fd table, no real process hierarchy — one spawn per
// `SPAWN` slot, and fd 0/1/2 are routed per task through `UserCtx::proc_slot`.

/// One spawned child: a pid and the scheduler task running it.
///
/// **All four stdio fields are gone**, and the last two went on 2026-09-11 with
/// the pipes themselves.
///
/// C2 slice 6 deleted `borrowed_io`/`console_io` and rewrote what `stdout_pipe`
/// was *for*; those three existed because a spawned child's stdio was routed *by
/// number*: every unbound fd 0/1/2 read or write asked the spawn row which pipe
/// (or console) served it, and the exit path closed the child's stdout end by
/// hand. Binding real descriptors in the child's own registered table retired
/// every one of those jobs. `stdout_pipe` outlived them by a slice because it
/// had acquired a second one — naming the child a `TIOCSWINSZ` on the parent's
/// stdout descriptor was meant for — and that moved to the child's registered
/// fd 1.
///
/// `stdin_pipe` outlived them all, for a real reason while it lasted: its write
/// end was reached by *path* (`sshd` opened `/proc/<pid>/fd/0`) and later by
/// *id*, and neither is a reference, so nothing refcounted it and the reap had
/// to drop it by hand or leak a pipe per spawn against a 64-pipe ceiling.
///
/// **There are no pipes.** A spawned child's stdio is one `ProcessChannel`,
/// which is what an AArch64 spawn has always been, and an `Arc` needs no reap to
/// notice it is finished. What the row still carries is the pair `waitpid` needs
/// and nothing else: which pid this slot is, and which task slot to unpublish.
#[derive(Clone, Copy)]
struct Spawn {
    pid: u32,
    /// The scheduler task slot running this child, recorded at spawn so the
    /// `waitpid` reap can remove the `THREAD_PID_MAP` entry it published
    /// (`reap_exec_process`). A `usize` is wider than `sched::MAX_TASKS` needs,
    /// but `usize::MAX` is not a sentinel here — every Spawn row has one.
    exec_slot: usize,
}

const SPAWN_SLOTS: usize = PROC_SLOTS - SPAWN_SLOT_BASE;

/// The spawn table: one row per spawned-process slot, as two arrays of atomics.
///
/// It was `static mut` reached through raw pointers, on the discipline the deleted
/// `PROCS` had: the only writers run inside a syscall and none yield while touching
/// it. That discipline is unchanged — the atomics do not make a *row* update
/// atomic, only make each word's access defined — but it no longer costs an
/// `unsafe` block per access. A row is "free" when its `pid` is 0 (no process has
/// pid 0), so `exec_slot` is written first and `pid` last (Release), and read in
/// the opposite order.
pub struct SpawnTable {
    pid: [AtomicU32; SPAWN_SLOTS],
    exec_slot: [AtomicUsize; SPAWN_SLOTS],
}

impl SpawnTable {
    const fn new() -> Self {
        Self {
            pid: [const { AtomicU32::new(0) }; SPAWN_SLOTS],
            exec_slot: [const { AtomicUsize::new(0) }; SPAWN_SLOTS],
        }
    }

    /// The row at `off`, or `None` when it is free or out of range.
    fn get(&self, off: usize) -> Option<Spawn> {
        let pid = self.pid.get(off)?.load(Ordering::Acquire);
        (pid != 0).then(|| Spawn { pid, exec_slot: self.exec_slot[off].load(Ordering::Relaxed) })
    }

    fn is_free(&self, off: usize) -> bool {
        self.get(off).is_none()
    }

    fn set(&self, off: usize, row: Spawn) {
        debug_assert!(row.pid != 0, "pid 0 marks a free spawn row");
        self.exec_slot[off].store(row.exec_slot, Ordering::Relaxed);
        self.pid[off].store(row.pid, Ordering::Release);
    }

    fn clear(&self, off: usize) {
        self.pid[off].store(0, Ordering::Release);
    }

    /// Rows in use.
    fn used(&self) -> usize {
        (0..SPAWN_SLOTS).filter(|&off| !self.is_free(off)).count()
    }

    /// The first free row's process slot (`SPAWN_SLOT_BASE + offset`).
    pub fn first_free_slot(&self) -> Option<usize> {
        (SPAWN_SLOT_BASE..PROC_SLOTS).find(|&s| self.is_free(s - SPAWN_SLOT_BASE))
    }

    /// The offset of the row naming `pid`.
    fn position_of(&self, pid: u32) -> Option<usize> {
        (0..SPAWN_SLOTS).position(|off| self.get(off).is_some_and(|row| row.pid == pid))
    }
}

pub static SPAWN: SpawnTable = SpawnTable::new();

/// The next pid.
///
/// **One counter, and it is `akuma-exec`'s.** This was a `static NEXT_PID` of
/// its own starting at 2 — which was safe only for as long as nothing on this
/// target drew from the shared counter, and since 5b both kernels register into
/// one process table. The first shared call that did draw from it
/// (`clone_thread`'s internal `allocate_pid`) returned **1**, init's pid, and
/// the new thread resolved through `THREAD_PID_MAP` to init's `Process` — so it
/// entered ring 3 at init's `context.pc`, which is 0. Two counters minting into
/// one table is the defect; this removes it rather than teaching the second
/// counter to dodge the first.
///
/// The "children start at 2" reservation survives as
/// `akuma_exec::process::reserve_pid_floor(2)`, called once at init — see that
/// function for why this target needs it and AArch64 does not.
///
/// It used to hand out **tids** as well ("one number space, as on Linux"). It
/// no longer does: a `clone` child's tid is its kernel thread slot, which is
/// what `clone(2)` returns and what every per-thread array is indexed by. See
/// `crate::thread::Thread::tid`.
#[must_use]
pub fn alloc_pid() -> u32 {
    akuma_exec::process::allocate_pid()
}

/// The `clone(2)` flag bits this kernel names.
///
/// Spelled out rather than reached through `akuma-syscalls-linux`: these are
/// architecture-independent Linux constants, and the one that matters here —
/// `CLONE_SETTLS` — pairs with an x86_64-specific *argument position*, so
/// keeping the two facts in one file is worth more than the deduplication.
pub mod clone_flags {
    // `CLONE_VM` left 2026-11-01: the thread/spawn/fork decision is
    // `akuma_syscalls_proc::clone::decode` now.
    pub const CLONE_THREAD: u64 = 0x0001_0000;
    pub const CLONE_SETTLS: u64 = 0x0008_0000;
    // `CLONE_PARENT_SETTID`, `CLONE_CHILD_SETTID` and `CLONE_CHILD_CLEARTID`
    // are **not** here since the `clone` fold: the shared
    // `akuma_exec::process::clone_thread` is the only reader of those three
    // now — it decides which pointer gets the tid and which gets the zero-and-
    // wake at exit — and a second copy of the bits beside a path that no longer
    // consults them is how the two drift. `trace_clone_flags` still names them,
    // from its own literal table, because a trace line has to name a bit
    // whether or not this file acts on it.
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

/// Record init's argv so `/proc/1` can describe it. Called once by [`run_init`].
fn set_init_cmdline<'a>(args: impl IntoIterator<Item = &'a [u8]>) {
    *INIT_CMDLINE.lock() = flatten_cmdline(args);
}


/// The pid of the process making the current syscall — what `/proc/self`
/// resolves to.
///
/// `1` for init and for anything not in the spawn table, matching
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
    // that slot.
    //
    // **It answers the `tgid`, not the task's own pid**, which is what
    // `getpid(2)` means on Linux: every thread of a group reports the group
    // leader's pid, and `gettid` is the per-thread number
    // (`akuma_exec::threading::current_thread_id`, which `gettid` answers from
    // since the 2026-09-11 fold).
    //
    // Until the `clone` fold the two were the same thing here by accident — a
    // `CLONE_THREAD` child had no `Process` of its own and was published into
    // `THREAD_PID_MAP` under its *leader's* pid, so "own pid" already meant
    // "group pid". The shared `clone_thread` gives each thread a real `Process`
    // with its own pid and `tgid = parent.tgid`, so the two diverge and the
    // distinction has to be made rather than inherited.
    //
    // Getting this wrong is silent and it bit immediately: this function is the
    // **futex namespace** (`crate::futex::namespace`, `(tgid, uaddr)`), so a
    // thread waking its parent enqueued on one key and woke another. The
    // `threadprobe` parent parked in `FUTEX_WAIT` forever and the boot suite
    // reported `0xffff…` — the never-stored sentinel — on two runs in three,
    // passing on the third only when it happened to see the value before
    // parking.
    //
    // Unmapped means init: the self-tests run before `run_init` registers
    // anything, and they are pid 1's work. That is the same answer the slot
    // walk gave for `slot < SPAWN_SLOT_BASE`.
    akuma_exec::process::current_thread_tgid_process().map_or(1, |(tgid, _)| tgid)
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
// * `signal_actions` empty, `signal_mask` 0 — the starting dispositions, which
//   is what a fresh image has on any Unix. Spelled "no signal delivery on this
//   target" until 2026-09-11, when both halves stopped being true together:
//   `rt_sigaction` is glue's and writes this table, and `crate::signal`'s
//   syscall-return epilogue reads it.
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
// `kernel_bkl_smp` is on and `ProcAddressSpace::lock()` really masks IRQs
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
pub fn register_exec_process(
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
    // The terminal to describe when this process asks `TIOCGWINSZ` — `None`
    // builds a fresh 24x80 one, and `sys_fork` passes the parent's `Arc`.
    //
    // A `fork` child inherits its parent's terminal on every Unix and on the
    // AArch64 kernel (`Process::fork` clones this same `Arc`); building it a
    // fresh one here meant an `sshd` session's window size stopped at the login
    // shell and every program the shell ran read the 24x80 default —
    // `AMD64_SSH_TERM_SIZE_NOT_PASSED.md`, the break its §5 fix order does not
    // list because the size never reached the shell either.
    //
    // Sharing the `Arc` rather than copying the size is what a live resize
    // needs: `sshd` writes the child's state on `window-change` and the running
    // full-screen program reads the same cell.
    term: Option<alloc::sync::Arc<Spinlock<akuma_tty::TerminalState>>>,
    // The process's **I/O** channel — `Process::channel`, not the per-thread
    // exit channel this function registers below. `Some` only for a
    // `SPAWN_FLAG_PTY` spawn, whose child needs a channel that reports
    // `is_terminal()` so the shared stdin sink runs its ISIG branch; `None`
    // falls back to the console's channel for a console-attached process and to
    // nothing for everything else.
    //
    // A parameter rather than a second reading of the fd table, because since
    // this argument exists the table can no longer answer the question: the
    // console test is "fd 0 is a `FileDescriptor::Stdin`", and a session child
    // that later takes its stdin from the channel too would answer it the same
    // way and be handed the *serial line's* channel.
    channel: Option<alloc::sync::Arc<akuma_exec::process::ProcessChannel>>,
) {
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::sync::Arc;

    use akuma_exec::process::{
        FreshProcess, LazyRegionMap, Process, ProcessChannel, ProcessMemory, SharedFdTable, register_channel,
        register_child_channel, register_process, thread_pid_map_insert,
    };

    let stack_bottom = ELF_STACK_TOP - (ELF_STACK_MAX_PAGES as u64 * 4096);

    // **Is this process attached to the console?**
    //
    // Hoisted out of the struct literal because two fields below depend on the
    // answer: a descriptor 0 that is a `FileDescriptor::Stdin` names the serial
    // line (`fd::console_end`'s descriptor spelling), so `init` on the serial
    // line and its descendants are console-attached and everything `sshd`
    // spawns is not — which is exactly the split the two fields want.
    //
    // **`channel.is_none()` is half the test and used not to be, and the fd
    // half stopped being sufficient the day the pipes were retired.** The note
    // here said "a spawned child's fd 0 is a `PipeRead` from `fd::bind_stdio`",
    // and that was true until C2 slice 6 gave a spawned child the shared
    // `SharedFdTable::with_stdio()` — whose fd 0 **is** `Stdin`. From then on
    // every `sys_spawn` child answered this test `true` and silently took the
    // console's *shared* `TerminalState` instead of the fresh one `sys_spawn`
    // passes `None` for and its own comment promises.
    //
    // What that cost, measured 2026-09-18: `busybox`'s line editor puts the
    // terminal in raw mode for each prompt (`ISIG` clear) and restores the
    // flags it read at startup before running a foreground job. Session 1 read
    // cooked flags, restored cooked, and `^C` killed its job. It then left the
    // *shared* cell raw, so **session 2 read raw flags as its "initial" state**
    // and restored raw — `ISIG` never came back, and `^C` was a `0x03` byte for
    // the rest of the guest's life, in every session, on both this target's
    // kernels of session stdio. `[ISIG-MISS]` in
    // `akuma_exec::process::write_to_process_stdin` is the tripwire that names
    // it; `docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §16 has the
    // trace.
    //
    // A process that carries its **own** channel is a session, not the console,
    // whatever its fd table says — so ask that first. The second field this
    // gates (`console::set_attached_pid`) was wrong for the same reason and in
    // the same direction: the serial line's keystrokes were being addressed to
    // the newest `ssh` session's shell.
    let fds = fds.unwrap_or_else(|| alloc::sync::Arc::new(SharedFdTable::with_stdio()));
    let console_attached = channel.is_none()
        && matches!(
            fds.table.lock().get(&0),
            Some(akuma_exec::process::FileDescriptor::Stdin)
        );

    // Group leader = self; no CLONE_THREAD groups in the table yet.
    let proc = Box::new(Process::fresh(FreshProcess {
        pid,
        parent_pid: ppid,
        // 5b slice 4: the **owning** address space, moved in from the loader.
        // Slice 1 registered a `new_shared` view here — non-owning, no `Drop`,
        // pointed at a root `PROCS` really owned — because two structures
        // claiming one L0 would double-free it. There is one structure now, so
        // the registration owns what it names, and `Process::drop` is what
        // frees a dead process's frames and page tables (through
        // `free_or_defer_as_frames`, which parks them while any core's `CR3` or
        // any preempted thread's saved context still stands on the L0).
        address_space: image.space,
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
        // 5b slice 4: **where ring 3 starts** — `entry`/`sp` become the saved
        // context, which is what that field has always meant — `UserContext::new(entry_point, stack_pointer)`
        // writes exactly `pc`/`sp`. Slice 1 registered it zeroed and noted
        // that amd64 "never `eret`s from it"; that stays true, and is
        // beside the point. `run_process` reads these two scalars back
        // (`current_entry_stack`) instead of indexing a second table, and
        // an `execve` rewrites them in this same lock so a re-entry cannot
        // pair a new entry point with an old stack.
        entry: image.entry as usize,
        sp: image.stack as usize,
        brk: image_top as usize,
        memory: ProcessMemory::new(
            image_top as usize,
            stack_bottom as usize,
            ELF_STACK_TOP as usize,
            crate::mm::MMAP_BASE,
        ),
        // Stated decision, not a default: see the module header — the page is
        // never read on this target and mapping it would leak 4 KiB/process.
        process_info_phys: 0,
        // 5b slice 4: the region list this target demand-pages from, moved in
        // from the loader. Empty for everything but a `fork` child, which
        // arrives carrying the parent's extents.
        mmap_regions: image.regions,
        lazy_regions: LazyRegionMap::new(),
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
        fds,
        // **The `akuma-threading` slot this process runs on.**
        //
        // `None` until 2026-09-11, deliberately, and the note that stood here
        // said why: this target's task lifecycle is `sched.rs`'s, and leaving
        // the field empty kept `unregister_process`'s thread-termination arm out
        // of it. What that also kept out was **signal delivery**:
        // `deliver_signal` collects its target tids from exactly this field, so
        // `all_tids` was empty, nothing was pended, nothing was woken — and it
        // still returned `true`, so `kill(2)` reported success and did nothing.
        //
        // Two facts make filling it safe rather than a reversal of that
        // reasoning. Since A1 the task slot **is** the `akuma-threading` thread
        // id on this target (`threading::current_thread_id` reads
        // `ArchHooks::current_slot`, which is `sched::current_task`), so there
        // is no second numbering to reconcile. And the field was never
        // consistently `None` anyway: a `fork` child and a `clone` thread both
        // go through the shared `spawn_child_thread_and_publish`, which has
        // always written `Some(tid)` here — so half this target's processes ran
        // with it set and nothing came of it. What was `None` was precisely the
        // half that could not be signalled: `init`, everything `sshd` spawns,
        // and every `execve`d image.
        thread_id: Some(task_slot),
        // A console process gets the console's **shared** line discipline, not a
        // fresh one — one serial line, one set of termios flags, exactly as a
        // tty behaves and as a `fork` child already inherits through `term`.
        // It is also what makes the pump's wake reach a parked reader:
        // `console::pump_once` fires the `input_waker` on this very cell, and
        // glue's `Stdin` arm registers on `current_terminal_state()`, which is
        // this field. Two objects here means a keystroke that wakes nobody.
        terminal_state: term
            .or_else(|| console_attached.then(crate::console::terminal_state).flatten())
            .unwrap_or_else(|| Arc::new(Spinlock::new(crate::console::default_terminal_state()))),
        // The spawner's credentials, copied: a spawn is credential-neutral,
        // exactly as on AArch64 (`akuma_exec::process::spawn`). `init` and the
        // self-test processes have no registered parent and start as root.
        creds: akuma_exec::process::lookup_process_shared(ppid)
            .map_or(akuma_exec::process::Creds::ROOT, |p| p.creds.get()),
        // **The console's channel, for a console-attached process; `None` for
        // everything else.**
        //
        // `None` here does not mean `current_channel()` answers `None` on this
        // target — it falls back to `get_channel(tid)`, and since the exit
        // channel landed (2026-09-10) that lookup succeeds for *every*
        // registered process. So the fd.rs comments that read "no process on
        // this target has one" describe a tree that no longer exists: what a
        // non-console process has is an **exit** channel, whose stdin FIFO
        // nothing fills, which is worse than none at all for a reader (it
        // parks rather than reporting EOF).
        //
        // Filling the field for the console case is what makes glue's
        // `Stdin`/`DevTty` arm correct here: it reads this channel, the pump
        // fills it, `/dev/tty` opens because `is_terminal()` holds, and the
        // arm's own `EAGAIN`, `EINTR`, raw/cooked branch and `EPOLLET` edge
        // re-arm come with it.
        channel: channel.clone().or_else(|| console_attached.then(crate::console::channel).flatten()),
    }));
    register_process(pid, proc);
    thread_pid_map_insert(task_slot, pid);
    // Tell the console pump who to address its keystrokes to, so `^C` goes
    // through the line discipline's `ISIG` branch instead of arriving as a
    // `0x03` byte. Only for a console-attached process; see
    // `console::ATTACHED_PID` for why "which one" does not matter.
    if console_attached {
        crate::console::set_attached_pid(pid);
    }

    // **The exit channel** — the AArch64 practice, adopted 2026-09-10.
    //
    // The two registrations are verbatim what the shared
    // `spawn_child_thread_and_publish` does for a `fork`/`vfork` child on the
    // other kernel, and they are what make `wait4` shared code rather than a
    // second implementation. This target had neither, and answered "has my
    // child exited?" from its own `SPAWN` row instead. Two things that could
    // not be answered that way arrive with them:
    //
    // * **`is_child_of_group`.** `wait*` on a process that is not your child
    //   must fail `ECHILD`, not block. The `SPAWN` row records a pid, not a
    //   parent link resolvable by thread group, so a multithreaded parent
    //   waiting from a non-leader thread had no correct answer — and Go's
    //   `os/exec` pidfd probe *requires* the `ECHILD` (`waitid(P_PIDFD)` on
    //   itself), where blocking deadlocks the caller against its own exit.
    // * **`EINTR`.** Signal delivery and `should_interrupt_blocking_syscall`
    //   both resolve through `get_channel(tid)`, which is why the per-thread
    //   registration is not optional decoration: without it a `wait4` parked
    //   in glue could not be interrupted.
    //
    // Deliberately **not** `Process::channel`, which stays `None`: that field
    // is a process's *I/O* channel, and on this target stdio is bound
    // descriptors over `crate::pipe`. This one carries an exit status and
    // nothing else, exactly as the shared spawn path's comment says.
    //
    // **A spawn passes its own channel in and all three registrations name
    // it**, which is what AArch64 does and is not a second shape: there, a
    // spawned process's `register_channel(tid, ch)` and `register_child_channel`
    // both take the I/O channel (`spawn.rs`'s thread body, glue's `sys_spawn`),
    // while a `fork`/`clone` child gets a fresh exit channel from
    // `spawn_child_thread_and_publish`. One channel is what lets the parent's
    // `ChildStdout(pid)` read the child's output *and* see EOF from
    // `has_exited()`; two would mean a reader parked on an empty FIFO that the
    // exit never marks.
    let exit_channel = channel.unwrap_or_else(|| alloc::sync::Arc::new(ProcessChannel::new()));
    register_channel(task_slot, exit_channel.clone());
    register_child_channel(pid, exit_channel, ppid);
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
pub fn reap_exec_process(pid: u32, task_slot: usize) {
    use akuma_exec::process::{
        pid_for_thread, reap_child_channel, thread_pid_map_remove, unregister_process,
    };
    unregister_process(pid);
    // The child channel goes with the registration, and it is done **here**
    // rather than at either call site because since 2026-09-10 this target has
    // a channel per child and **two** reapers: this file's `sys_waitpid`
    // (Akuma's private 303, which `sshd`'s bridge polls non-blockingly) and
    // glue's `wait4`. Whichever gets there must do the whole job, or a reap
    // through one of them leaves a `CHILD_CHANNELS` entry — and the
    // `Arc<ProcessChannel>` it holds — for the life of the boot. Measured: ~200
    // bytes of kernel heap per child, which `amd64_ring3_check`'s `heap` column
    // reports as a rising drift across 60 ssh sessions.
    //
    // Inside this function rather than beside it, because it has two callers
    // and the second one is the path that is easy to forget: `sys_waitpid`'s
    // fallback for a child the process table knows and no `SPAWN` row names.
    // `reap_child_channel` declines while the channel still holds unread
    // stdout, so a parent mid-drain is not robbed of it.
    reap_child_channel(pid);
    if pid_for_thread(task_slot) == Some(pid) {
        thread_pid_map_remove(task_slot);
    }
}

/// x86_64 syscall numbers already reported by [`report_unknown_syscall`].
///
/// `u64::MAX` is the empty marker, not `0` — x86_64 `0` is `read`.
static UNKNOWN_SYSCALLS: [AtomicU64; 32] = [const { AtomicU64::new(u64::MAX) }; 32];

/// Say, once per number, that a syscall arrived with no row in
/// `akuma_syscalls_abi`'s table.
///
/// **This class of gap has cost three sessions.** `chmod`/`chdir`/`fchmod`/
/// `fchmodat` had implementations in `akuma-syscalls-glue` and no numbers, so
/// `git clone` failed at a `chmod` and `top` at a `chdir`; x86_64 `symlink`
/// dispatched to `sys_utimensat` and `ln -s` silently created nothing for
/// months (`docs/archive/AKUMA_AMD64_C1_DISPATCH_VOCABULARY.md`). Every one of
/// them reached userspace as `Function not implemented` from a program that
/// then blamed itself — `rust-lld` reported `cannot open output file`, `gcc`
/// reported `no input files` — and the kernel said nothing at all.
///
/// One line per distinct number, on the console, is the whole fix: it names the
/// number the ABI table is missing, at the moment it is missed. Bounded to 32
/// distinct numbers and printed once each, so a program probing for a syscall
/// in a loop costs one line and a runaway cannot flood the console that is
/// meant to survive things going wrong.
fn report_unknown_syscall(nr: u64) {
    let mut claimed = false;
    for slot in &UNKNOWN_SYSCALLS {
        match slot.compare_exchange(u64::MAX, nr, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                claimed = true; // ours to report
                break;
            }
            Err(seen) if seen == nr => return, // already reported
            Err(_) => {}                       // someone else's; try the next slot
        }
    }
    if !claimed {
        // The table is full: thirty-two distinct numbers have already been
        // named, which is a finding in itself and more than enough to act on.
        // Falling through to print here instead would flood the console on
        // exactly the kernel that is missing the most.
        return;
    }
    serial::puts("[syscall] no row for x86_64 nr=");
    serial::put_dec(nr);
    serial::puts(" — returning ENOSYS (add it to akuma-syscalls-abi's table)\n");
}

/// Read a NUL-terminated string from user memory, bounded.
fn user_cstr(ptr: u64, max: usize) -> Option<alloc::vec::Vec<u8>> {
    crate::uaccess::read_cstr(ptr, max)
}

/// Parse a NULL-terminated array of C-string pointers, bounded at `max`
/// entries — `None` if the caller's array is longer than that.
///
/// **`None` rather than a short list, and that distinction is the whole
/// function.** Until 2026-09-12 this returned whatever fitted, so an `execve`
/// with more arguments than [`loader::MAX_ARGV`] started the program with a
/// command line that was well-formed and wrong. `rustc` and `collect2` both
/// build linker invocations of forty-odd arguments; the linker ran without its
/// `-o` and without its inputs, and said so in its own vocabulary
/// (`no input files`, `cannot open output file a.out`) with nothing pointing
/// at the kernel. `E2BIG` is what Linux answers and is impossible to
/// misattribute.
/// One `execve`'s argument budget, across argv and envp together
/// (`akuma_syscalls_proc::exec`): the initial stack is where every string lands
/// ([`loader::build_stack`]), so half of it is the honest ceiling on their total.
fn exec_arg_budget() -> akuma_syscalls_proc::exec::Budget {
    akuma_syscalls_proc::exec::Budget::new(
        loader::MAX_ARGV + loader::MAX_ENVP,
        akuma_config::MAX_ARG_STRLEN,
        ELF_STACK_PAGES * 4096 / 2,
    )
}

fn user_strv(
    ptr: u64,
    max: usize,
    budget: &mut akuma_syscalls_proc::exec::Budget,
) -> Option<alloc::vec::Vec<alloc::vec::Vec<u8>>> {
    // `budget` ([`exec_arg_budget`]) is charged as each string is read.
    // Refusing here rather than at the stack build keeps the kernel from copying
    // megabytes out of ring 3 to build an argv that cannot be placed — and
    // `build_stack` still has its own `rsp < base` check, because this one is
    // about the copy and that one is about the arithmetic.
    let mut out = alloc::vec::Vec::new();
    if ptr == 0 {
        return Some(out);
    }
    let entry = |i: usize| crate::uaccess::read_val::<u64>(ptr + (i as u64) * 8).unwrap_or(0);
    for i in 0..max {
        // A bad array pointer ends the list, like a NULL entry would.
        let p = entry(i);
        if p == 0 {
            return Some(out);
        }
        // **An over-long string fails the whole call; it does not end the
        // list.** This read was capped at 512 bytes and answered `None` by
        // returning what it had so far, which is the silent-truncation shape
        // this tree has paid for twice: session 3's `argv` cap of 16 ran every
        // linker without its inputs, and glue's own `parse_argv_array` had the
        // identical `break` (rustc saw `--check-cfg` with its value gone).
        // Measured 2026-09-13: `sh -c "<667-byte command>"` lost the command
        // and the child reported `sh: -c requires an argument` — a 512-byte
        // argument is nothing unusual in a `cargo`-generated `rustc` line.
        //
        // `MAX_ARG_STRLEN` is the shared cap glue uses, so the two kernels
        // refuse the same argument rather than one of them mangling it.
        let s = user_cstr(p, akuma_config::MAX_ARG_STRLEN)?;
        budget.charge(s.len()).ok()?;
        out.push(s);
    }
    // `max` entries and still no terminator: the caller has more to say than
    // this target can place on the initial stack.
    if entry(max) != 0 {
        return None;
    }
    Some(out)
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
/// `Image::from_elf_argv_envp`'s rejection strings, mapped to the errno Linux
/// would return — the honest replacement for the blanket `ENOMEM` that stood
/// here, which turned a missing `PT_INTERP` file into
/// `sh: ./hello: Out of memory` (`docs/archive/RUST_TOOLCHAIN_AMD64.md`
/// § "Open" item 3). Keyed on the strings `loader::elf_err` produces from
/// `akuma-elf`'s `ElfError`; a string this does not recognise is `ENOEXEC` —
/// "the image was rejected", the default a loader rejection deserves — rather
/// than a resource lie. The string is still printed on the serial console, so
/// nothing is lost by the narrowing.
fn exec_load_errno(e: &str) -> u64 {
    use crate::fd::errno;
    if e.contains("out of memory") {
        // The one string `elf_err` produces for `ElfError::OutOfMemory`.
        return errno::ENOMEM;
    }
    if e == "File read failed" || e == "Short read" {
        // `akuma-elf`'s path source: the file stopped reading part-way through
        // the load (a device error, or a file truncated under it). Before the
        // loader streamed, the same failure surfaced from the whole-file read
        // as `EIO`, and it still should.
        return errno::EIO;
    }
    if e.contains("interpreter") {
        // "Cannot read interpreter" — the `PT_INTERP` file is missing or
        // unreadable, which is what `./hello` naming `/lib/ld64.so.1` hit.
        return errno::ENOENT;
    }
    errno::ENOEXEC
}

/// A path from ring 3, resolved the way the calling process means it.
///
/// **`execve("./prog")` must mean the process's own directory, not `/`.** This
/// target's `execve` and `sys_spawn` handed the raw string to
/// `crate::fs::read_image`, which resolves symlinks and nothing else, so every
/// relative path was read as if it started at the root. Two failures, and the
/// second is the dangerous one:
///
/// * `cargo build && ./target/debug/prog` reported `not found` for a binary
///   `ls` had just listed in that very directory — relative `open` goes through
///   glue's `sys_openat`, which resolves against `cwd`, so the two syscalls
///   disagreed about what a path meant.
/// * With a same-named file at the root, the exec **silently ran the wrong
///   program**: `cd /tmp/hi/target/debug && ./hi` ran `/hi`, left over from an
///   earlier session, and printed its output as if it were the build's.
///
/// The rule is glue's `sys_execve`, restated here because this kernel does not
/// route `execve` through it: absolute paths are already absolute, a relative
/// path is joined to `Process::cwd`, and a caller with no registered process
/// (a kernel task running the boot suite) keeps what it passed.
///
/// Not PATH lookup — `execve(2)` does not search `PATH` and neither does this.
fn process_relative_path(path: &str) -> alloc::string::String {
    if path.starts_with('/') {
        return alloc::string::String::from(path);
    }
    match current_process() {
        Some(proc) => akuma_vfs_glue::resolve_path(&proc.cwd, path),
        None => alloc::string::String::from(path),
    }
}

fn sys_execve(path_ptr: u64, argv_ptr: u64, envp_ptr: u64) -> u64 {    use crate::fd::errno;

    let slot = current_proc_slot();
    if slot >= PROC_SLOTS {
        // Not a slotted user task — nothing to replace. (`current_proc_slot`
        // answers `usize::MAX` off a user task, which this same bound catches.)
        return errno::ENOSYS;
    }

    // The path against the process's `cwd`, and argv/envp under one budget:
    // glue's `execve::read_request`, shared with Taoxichuan (2026-11-01).
    let mut budget = exec_arg_budget();
    let req = match akuma_syscalls_glue::execve::read_request(
        path_ptr,
        argv_ptr,
        envp_ptr,
        loader::MAX_ARGV,
        loader::MAX_ENVP,
        &mut budget,
    ) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let (path, argv_owned, envp_owned) = (req.path, req.argv, req.envp);
    do_execve(slot, path, argv_owned, envp_owned)
}

/// The image-loading half of `execve`, split out so a `#!` script can recurse
/// into it with the interpreter's path and a rebuilt argv — same shape as
/// `sys_execve`'s split into this and [`sys_execve`] itself, and the same
/// split AArch64's `akuma-syscalls-glue::proc` already has
/// (`sys_execve`/`do_execve`). `slot` is threaded through rather than
/// recomputed: it names the *calling task*, which a shebang hop does not
/// change.
fn do_execve(
    slot: usize,
    path: alloc::string::String,
    argv_owned: alloc::vec::Vec<alloc::vec::Vec<u8>>,
    envp_owned: alloc::vec::Vec<alloc::vec::Vec<u8>>,
) -> u64 {
    use crate::fd::errno;

    // Execute permission (`EACCES`), the same check AArch64's glue `do_execve`
    // makes — this target has its own exec path, so it does not get that one
    // for free. Against the symlink-resolved path, as the image read below
    // resolves it: an apk wrapper is a link, and the link's own mode is 0777.
    // A `#!` hop re-enters here with the interpreter, which is checked too.
    if let Err(e) = akuma_syscalls_glue::perm::check(
        &akuma_vfs_glue::resolve_symlinks(&path),
        akuma_isolation::dac::MAY_EXEC,
    ) {
        return e;
    }

    // Mark the file as being executed BEFORE its bytes are read (`ETXTBSY`, `docs/archive/ETXTBSY_TEXT_BUSY.md`), so it cannot be
    // rewritten between the read and the mark. It must reach `Process::set_text` or be dropped before this function can `eret`: the
    // frame is abandoned on success, and a pin left in it would keep the file busy forever.
    let Ok(text) = akuma_vfs_glue::text_pin(&akuma_vfs_glue::resolve_symlinks(&path)) else {
        return errno::ENOMEM;
    };

    // The image read runs BKL-free per `EXEC_BKL_DROP_ENABLED` — on a USB disk
    // whose transfer stalled, this read is the multi-second BKL hold behind the
    // `[BKL] stuck` storm of 2026-09-11 (`exec_runtime.rs` § "The exec-side
    // BKL drop"). Everything after it needs the BKL again (the image switch
    // edits live page tables, and the shootdown argument assumes the BKL is
    // outermost), so the guard is scoped to exactly this read.
    //
    // The errno is the table's, not a blanket ENOENT: during the stall,
    // `read_file` failing with `IoError` on cache-cold binaries printed as
    // "not found" for files that exist — `/bin/ls` "missing" while
    // `/bin/busybox` (block-cache-hot) ran — which sent the investigation
    // hunting for a broken rootfs.
    //
    // Only the head is read here, since 2026-10-08: the loader reads the image
    // off the file a window at a time (`loader::load_path`), so the binary's
    // size is no longer a kernel-heap demand (`fs::read_image_head`). The head
    // read resolves symlinks — the raw read once handed the *link text* to the
    // loader, and every apk-installed wrapper is a link.
    let head = match crate::exec_runtime::bkl_free_io(|| crate::fs::read_image_head(&path)) {
        Ok(head) => head,
        Err(e) => return akuma_syscalls_glue::fs::fs_error_to_errno(e),
    };
    let image = head.head();

    // `#!interpreter [arg]` — added 2026-09-20; this target had never had any
    // shebang handling and handed the raw script bytes straight to the ELF
    // loader, which rejected them as `ENOEXEC` (docs/README.md's symptom
    // matrix, "execve of a `#!` script fails with Exec format error"). A shell
    // hides this by retrying a non-ELF child itself, so it only showed up the
    // moment something else — `rustc` execing its linker, `apk`'s hooks —
    // called `execve` directly. Same parser and argv-construction rule as the
    // AArch64 kernel's `akuma-syscalls-glue::proc::exec_shebang`, both
    // host-tested in `akuma-exec`'s `shebang_tests`, so the two cannot drift.
    if image.len() >= 2 && image[0] == b'#' && image[1] == b'!' {
        let line = &image[..image.len().min(akuma_exec::process::SHEBANG_MAX)];
        let Some((interpreter, shebang_arg)) = akuma_exec::process::parse_shebang(line) else {
            return errno::ENOENT;
        };
        // Owned, before `image` (which `interpreter`/`shebang_arg` borrow) is
        // dropped — the recursive `do_execve` below may `eret` on success and
        // abandon every frame here, so nothing large may still be borrowed.
        let interp_argv0 = alloc::string::String::from(interpreter);
        let interp_arg = shebang_arg.map(alloc::string::String::from);
        drop(head);

        // Symlink-resolved, like the AArch64 side: a `#!/bin/sh` reached
        // through a symlink must still load the real interpreter, and
        // `interp_argv0` (as written in the `#!` line) stays what argv[0]
        // becomes — collapsing the two loses the interpreter's identity for a
        // multi-call binary that dispatches on argv[0] (busybox).
        let interp_path = akuma_vfs_glue::resolve_symlinks(&interp_argv0);

        let mut new_argv: alloc::vec::Vec<alloc::vec::Vec<u8>> = akuma_exec::process::shebang_hop(
            &interp_argv0,
            interp_arg.as_deref(),
            &path,
            &[],
        )
        .into_iter()
        .map(alloc::string::String::into_bytes)
        .collect();
        if argv_owned.len() > 1 {
            new_argv.extend(argv_owned.into_iter().skip(1));
        }
        drop(path);
        // The script is data; its interpreter is what runs, and the recursion marks that.
        drop(text);

        return do_execve(slot, interp_path, new_argv, envp_owned);
    }

    let argv_refs: alloc::vec::Vec<&[u8]> =
        argv_owned.iter().map(alloc::vec::Vec::as_slice).collect();
    let envp_refs: alloc::vec::Vec<&[u8]> =
        envp_owned.iter().map(alloc::vec::Vec::as_slice).collect();

    let (next, ld) = match Image::from_elf_path_argv_envp(&head.resolved, &argv_refs, &envp_refs) {
        Ok(p) => p,
        Err(e) => {
            serial::puts("  [execve] load failed: ");
            serial::puts(e);
            serial::puts("\n");
            // The image was rejected; the caller's own image is untouched, so
            // this is a real errno return, not a leave.
            return exec_load_errno(e);
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
    // **Resolved through symlinks**, so `/proc/<pid>/exe` names the binary
    // and not the spelling. Chromium starts every child with
    // `execve("/proc/self/exe", …)` and then finds its own files next to
    // `readlink("/proc/self/exe")`; recording the literal path made that
    // `/proc/self/exe` again, the directory `/proc/self`, and the child died
    // `FATAL: Error loading V8 startup snapshot file` (2026-10-08). Resolved
    // here, before the swap, `/proc/self` is still the *caller*, whose binary
    // is exactly what a re-exec of itself runs.
    let new_name = akuma_vfs_glue::resolve_symlinks(&path);

    let pid = current_pid();
    let new_root = next.space.ttbr0();
    let new_brk = ld.end_va;
    let new_entry = next.entry;
    let new_stack = next.stack;
    let stack_bottom = (ELF_STACK_TOP - (ELF_STACK_MAX_PAGES as u64 * 4096)) as usize;

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
    // The old image's writable `MAP_SHARED` file mappings die with it: write
    // them back and release their shared pages now, while the old address
    // space is still the current one — its `Drop` inside `install_image` would
    // do neither. Same call the exit epilogue makes.
    crate::mm::release_shared_write_mappings();

    // Built **before** the hold below. `with_process` runs its closure with
    // interrupts disabled and states that it must not allocate on the heap, so
    // everything that allocates is done here and only moved in there.
    let new_args: alloc::vec::Vec<alloc::string::String> = exec_cmdline
        .split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| alloc::string::String::from_utf8_lossy(a).into_owned())
        .collect();

    // Step 1: install the new image on the registered process — **the shared
    // `Process::install_image` since 2026-09-11**, which is the half of
    // `execve` that is about the process rather than about the ELF.
    //
    // The load above stays this target's (`Image::from_elf_argv_envp`, i.e.
    // `akuma-elf`'s `load_elf` plus `loader::build_stack`): C1 step 6 kept that
    // stack builder deliberately, and the shared `replace_image` begins by
    // loading its own way. `ImageInstall` is the seam between the two, and it
    // carries only what the install needs to know — no `ImageSource`, no
    // deferred segments (this target maps eagerly, so the slice is empty).
    //
    // **Three POSIX obligations arrive with it that this function did not
    // perform**, and none is cosmetic:
    //
    // * `clear_child_tid` is reset. A `CLONE_CHILD_CLEARTID` address from the
    //   *previous* image would otherwise be zeroed-and-woken at exit — a write
    //   to whatever the new program has at that VA.
    // * custom signal handlers go back to `SIG_DFL` (`SIG_IGN` preserved).
    //   Recorded here as "inert today, correct the moment `rt_sigaction` stops
    //   being a stub"; it stopped on 2026-09-11 and this is live — an `execve`
    //   that kept the old image's handler addresses would jump the new program
    //   into whatever is at that VA on its first signal.
    // * the alternate signal stack is disabled; it pointed into the old space.
    //
    // The old address space and the old region list are dropped **inside**, not
    // returned to be dropped here. That was this function's own arrangement for
    // a good reason — `UserAddressSpace::drop` frees every user frame and page
    // table, which is not work for a closure `with_process` runs with
    // interrupts disabled — and the reason does not apply to `install_image`:
    // it takes `&self` and interior locks, at normal priority. It also
    // `deactivate()`s first, so the hardware is off the old tables before they
    // are freed, which is the ordering this function achieved with
    // `set_current_space_root` and which is now stated where it is relied on.
    // **The thread group leader's `Process`, not the calling task's own.** The
    // block this replaced resolved through `with_process(current_pid(), ..)`,
    // and `current_pid()` is the *tgid* since the `clone` fold — so this is the
    // same process it always installed onto, named through the accessor that
    // says which one it means. `current_process()` would be the own-half and
    // would install the new image onto a non-leader thread's own `Process`,
    // leaving the group leader running the program that was just replaced.
    //
    // `execve` replaces an address space, which is a property of the thread
    // *group*, so the leader is right on both counts — the same reasoning
    // `current_mm_process` carries for the fault path.
    let Some((_, proc)) = akuma_exec::process::current_thread_tgid_process() else {
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
    if let Err(e) = proc.install_image(akuma_exec::process::ImageInstall {
        address_space: next.space,
        entry_point: new_entry as usize,
        sp: new_stack as usize,
        brk: new_brk as usize,
        stack_bottom,
        stack_top: ELF_STACK_TOP as usize,
        mmap_floor: crate::mm::MMAP_BASE,
        args: &new_args,
        // This target maps every segment eagerly (`loader::load`), so there is
        // nothing for the shared lazy path to defer.
        deferred_segments: &[],
    }) {
        serial::puts("  [execve] install failed: ");
        serial::puts(&e);
        serial::puts("\n");
        return errno::ENOMEM;
    }
    // The new image's executable is busy for as long as it runs (the old image's mark is released).
    proc.set_text(text);
    // `install_image` cleared the region list; the stack's demand-grown part is
    // one region, registered as at first exec (`Image::with_stack`).
    crate::mm::register_lazy_stack(ELF_STACK_TOP, ELF_STACK_PAGES, ELF_STACK_MAX_PAGES);
    // The display name is the syscall layer's, on both kernels —
    // `install_image`'s doc says so, because it is the *resolved path* here and
    // `argv[0]` would be a name the caller chose rather than something that
    // opens. `/proc/<pid>/exe` reports `exe`, which only this line and spawn
    // write — `name` is the `comm` that `prctl(PR_SET_NAME)` may rename later.
    // (`/proc` identity and the close-on-exec sweep are glue's
    // `execve::commit`, below, shared with Taoxichuan.)

    // Image replacement committed — *now* close the close-on-exec descriptors.
    // POSIX's point of no return: a successful `execve` closes every
    // `FD_CLOEXEC` fd, and a failed one (every `return errno::*` above) leaves
    // them untouched, which is why this sits after the install rather than
    // before it.
    //
    // **This target had no sweep at all**, and the cost was not a leak — it was
    // a hang. `git`'s `start_command` creates a notify pipe whose write end is
    // `FD_CLOEXEC`, forks, closes the parent's copy of that end, and then
    // *blocks in `read()`* on the read end: a successful exec closes the
    // child's copy, the pipe runs out of writers, and the read returns EOF,
    // which is how the parent learns the exec worked. With the write end
    // surviving the exec there is no EOF, so `git` never returns from that read
    // and never sends `capabilities` to the helper it just started — both sides
    // parked, the helper on a stdin nobody will write.
    //
    // Traced 2026-09-18: `nr=3 a1=0x8` (close 8) then `nr=0 a1=0x7` (read 7),
    // never returning, with the per-pipe dump showing `bytes=0 writers>0` on
    // every live pipe. The AArch64 kernel has done this since its own
    // `execve` was written (`akuma-syscalls-glue`'s `sys_execve`); the sweep is
    // shared code and this call site is what was missing.
    // `docs/archive/AMD64_TRASHCAN_ISSUES.md` §1.
    let new_args_bytes: alloc::vec::Vec<alloc::vec::Vec<u8>> =
        new_args.iter().map(|a| a.clone().into_bytes()).collect();
    akuma_syscalls_glue::execve::commit(proc, &new_name, &new_args_bytes);

    // A new program in an existing slot starts with a clean group: a stale
    // `exit_group` flag from the image just replaced would kill its first
    // thread at its first syscall.
    crate::thread::clear_group_exiting(slot);
    // Step 2: `CR3` onto the new space, and record it on the task slot so the
    // next switch into this task installs the same thing.
    //
    // `install_image` left the hardware on the **boot** root
    // (`UserAddressSpace::deactivate`) rather than on the new space: it has no
    // opinion about which address space a caller wants active afterwards, and
    // on the other kernel the `eret` path decides. Here it is this line, and it
    // must run before ring 3 is re-entered — which `run_process`'s loop does
    // immediately after this syscall returns.
    //
    // The old space and the old region list are already gone, dropped inside
    // the install *after* that `deactivate` — which is the ordering the two
    // explicit `drop`s that used to stand here were arranging by hand.
    crate::sched::set_current_space_root(new_root);

    // **Return into the new image**, rather than leaving ring 3 and being sent
    // back in by a loop.
    //
    // `user_rip`/`user_rsp` are where the syscall return path takes the program
    // counter and stack from — the first is true only since this change. The
    // ordinary return pops the `rcx`/`r11` that the `syscall` instruction
    // itself delivered; `[rax + 32]` was written for `vfork`'s benefit alone
    // and nothing read it on the way out. `exec_pending` now selects
    // `.Lexec_return`, which reads both, zeroes the register file and
    // `sysret`s. `leave` is deliberately **not** set: leaving ring 3 is what
    // made `execve` need a loop to get back in.
    //
    // Under the BKL, interrupts off inside a syscall; only this task reads these
    // fields.
    let armed = crate::sched::with_current_uctx(|u| {
        u.user_rip = new_entry;
        u.user_rsp = new_stack;
        u.exec_pending = 1;
    });
    if armed.is_none() {
        // Nothing to return through. The old image is already gone, so there is
        // no program to return to either; say so rather than `sysret`ing
        // somewhere arbitrary.
        serial::puts("  [execve] no UserCtx on this task\n");
        return errno::ENOSYS;
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

    // **The fold.** Everything this function used to do by hand —
    // build the child's address space, copy the descriptor table, register the
    // process, spawn and seed the task, publish — is
    // `akuma_exec::process::fork_process`, and has been reachable from here one
    // piece at a time since the ring-3 entry seam started:
    //
    // * slice 1 gave `UserContext` an x86_64 arm, so the function compiles here;
    // * slice 2 gave `Process::run` the `enter_user` hook, so a child spawned by
    //   shared code reaches ring 3 through this target's returning lifecycle;
    // * slice 3 put every child's exit `ProcessChannel` in `CHILD_CHANNELS`, so
    //   `ChildReaping::Reapable` has somewhere to register;
    // * slice 4 made the memory pass a hook, so step 4 speaks x86 page tables;
    // * slice 5 gave `get_saved_user_context` and
    //   `spawn_user_closure_initializing` real x86_64 arms, so steps 6 and 7
    //   stop failing.
    //
    // What is left here is the pid and the errno translation. The `SPAWN` row,
    // the `CR3` root and the `UserCtx::proc_slot` — the three things shared code
    // has no concept of — are bound through `ExecRuntime::bind_child_task`
    // below, in the one window where the child provably has not executed an
    // instruction.
    //
    // Three behaviours arrive with the shared path and none is silent:
    //
    // * **the child inherits the parent's `brk`.** This target passed
    //   `image_top: 0` and said so; `Process::inherit_from` carries the
    //   parent's, which is what Linux does and what the CoW share pass makes
    //   true — the child owns a copy of the parent's heap pages, so a `brk`
    //   naming them is naming its own memory now.
    // * **`signal_actions` is a `clone_for_fork` copy**, not a fresh table.
    //   POSIX; this target had no dispositions to carry, so it is a gain that
    //   costs nothing today and is correct the moment `sigaction` works.
    // * **`LifecycleGuard`** wraps the whole thing. A no-op unless
    //   `kernel_bkl_smp`, which this target does not build.
    //
    // The parent-slot bound this used to check (`>= PROC_SLOTS`) is gone with
    // the hand-rolled slot search; `current_process()` inside `fork_process` is
    // the same question asked of the authoritative table.
    crate::kvm::api::note_fork();
    let child_pid = alloc_pid();
    match akuma_exec::process::fork_process(child_pid, 0) {
        Ok(pid) => u64::from(pid),
        Err(e) => {
            // Named, not swallowed. Every failure inside `fork_process` is a
            // `&'static str` and `sh: can't fork: Out of memory` names the
            // wrong resource for most of them — the `SPAWN` table filling up
            // being the one this target hits first.
            serial::puts("  [fork] ");
            serial::puts(e);
            serial::puts("\n");
            errno::ENOMEM
        }
    }
}

/// `clone(CLONE_VM|CLONE_VFORK)` — the `posix_spawn` child, served as a fork
/// that enters on the caller's clone stack. See the `Syscall::Clone` arm for
/// the full mechanism and the two deliberate divergences; this is only the
/// errno translation, matching [`sys_fork`].
fn sys_spawn_clone(child_stack: u64) -> u64 {
    use crate::fd::errno;
    let child_pid = alloc_pid();
    match akuma_exec::process::fork_process(child_pid, child_stack) {
        Ok(pid) => u64::from(pid),
        Err(e) => {
            serial::puts("  [spawn-clone] ");
            serial::puts(e);
            serial::puts("\n");
            errno::ENOMEM
        }
    }
}

/// **`ExecRuntime::bind_child_task` on this target** — give a freshly spawned
/// `fork`/`vfork` child the three things `akuma-exec` has no concept of, in the
/// window after its task slot exists and before it can be scheduled.
///
/// All three are keyed on the **task slot**, which is why this cannot happen
/// before the spawn, and all three must be in place before publication, which
/// is why it cannot happen after `mark_thread_ready`:
///
/// * **`space_root`** — the page-table root the scheduler writes to `CR3` on
///   every switch into this task. The crate's spawn leaves it `0` (kernel
///   `CR3`) because it has no `Process` to read a root from, and the first
///   switch into an unbound child would run ring-3 code against the kernel's
///   tables.
/// * **the `SPAWN` row** — this target's per-process exit record.
/// * **`UserCtx::proc_slot`** — the index naming that row, which
///   [`run_process`] reads back for `thread::drain` and `spawn_record_exit`.
///   [`proc_entry`] reads it too, and refuses to enter ring 3 without it.
///
/// The row is swept for reaped predecessors first, for the reason `sys_fork`
/// swept before it: glue's `wait4` frees the process, not the row (see
/// [`sweep_reaped_spawn_rows`]), so without this the table fills with rows for
/// processes that no longer exist and `fork` reports failure on a machine with
/// gigabytes free.
///
/// **`stdin_pipe`/`stdout_pipe` are `None` and that is not a stub.** A `fork`
/// child's fd 0/1/2 are *names* for the parent's descriptions, copied by
/// `Process::inherit_from`'s `clone_deep_for_fork` and released by this child's
/// own table at exit. It owns no pipe of its own; only `sys_spawn` creates one.
pub fn bind_child_task(
    task_slot: usize,
    child: &akuma_exec::process::Process,
    kind: akuma_exec::process::ChildKind,
) -> Result<(), &'static str> {
    // The root first: **both** kinds need it, and a thread's is the parent's
    // own — `clone_thread` builds the child a `new_shared` view of the same
    // PML4, so `ttbr0()` answers the parent's root and the scheduler installs
    // exactly the address space the thread is meant to share.
    crate::sched::set_task_space_root(task_slot, child.address_space.ttbr0());

    // A `clone` child is not a process and must not get a `SPAWN` row: that row
    // is the target's per-*process* exit record, and one written for a thread
    // would make `spawn_record_exit` publish an exit status — a `wait4` in the
    // parent would reap a process that is still running. What a thread needs
    // instead is a `THREADS` row and a `UserCtx::thread_slot`; see
    // `crate::thread::bind_clone_child` for the four questions that answers.
    if kind == akuma_exec::process::ChildKind::Thread {
        return crate::thread::bind_clone_child(
            task_slot,
            current_proc_slot(),
            child.clear_child_tid.load(Ordering::Relaxed),
        );
    }

    // **A new process in this slot must not inherit the old one's exit.**
    // `GROUP_EXIT` outlives the process that set it — `drain` sets it and
    // nothing cleared it on the plain `fork` path, where the child is bound to
    // a *recycled* slot the way `execve`/`spawn` are (and only those two
    // cleared it). A fork child landed on such a slot ran fine itself —
    // `should_leave_now` is false for a main thread — but every thread it
    // ever `pthread_create`d saw `group_exiting` true at its **first** syscall,
    // answered `EINTR` and left before executing a user instruction. Measured
    // 2026-09-12 with a fork → spawn-8-parkers child: `started=0`, and the
    // console showed every worker's `run_thread` returning `-4` immediately —
    // `/probes/segvgroup` failing its worker barrier from round 0. Threads
    // must not clear this: they *join* the running group, which may genuinely
    // be exiting.

    // Collect any row the reaper left behind before looking for a free one.
    sweep_reaped_spawn_rows();

    let slot = SPAWN.first_free_slot();
    let Some(slot) = slot else {
        // A bare "out of memory" here reaches the user as `sh: can't fork: Out
        // of memory`, which names the wrong resource: the table is full, and
        // the machine may have gigabytes free. Say which, and how full.
        let spawn_used = SPAWN.used();
        serial::puts("  [fork] no free process slot: SPAWN ");
        serial::put_dec(spawn_used as u64);
        serial::puts("/");
        serial::put_dec(SPAWN_SLOTS as u64);
        serial::puts(" registered ");
        serial::put_dec(akuma_exec::process::process_count() as u64);
        serial::puts("\n");
        return Err("fork: no free SPAWN row");
    };

    // Under the BKL, and the task is unpublished.
    SPAWN.set(slot - SPAWN_SLOT_BASE, Spawn { pid: child.pid, exec_slot: task_slot });
    crate::thread::clear_group_exiting(slot);
    crate::thread::clear_group_exit_status(slot);
    // `seed_proc_slot` alone leaves `UserCtx::thread_slot` untouched. Every
    // OTHER caller of this task-slot claim is a `fork`/`vfork`/`clone` child
    // reached through `spawn_user_thread_initializing`'s two-phase claim,
    // which never hands out a slot a `clone` thread is still occupying — the
    // pool's own bookkeeping keeps those two kinds of slot apart in practice.
    // `spawn_ext` is the first caller that claims a slot through the
    // *one-phase* `spawn_user_thread_fn_for_process` primitive instead, which
    // has no `bind_child_task`-shaped seed step of its own at all, so this is
    // the only place anything for this target resets `thread_slot` for a
    // process bind. Left stale, a recycled slot's old `clone` value would send
    // `enter_ring3` down the `crate::thread::run_thread` branch for a plain
    // process. Added alongside the `spawn.rs` lock-ordering fix
    // (`docs/archive/AKUMA_AMD64_BOX_SPAWN_EXT.md`) that actually resolved the
    // silent full-VM crash `spawn_ext` hit before both landed — this line's
    // own effect was not isolated from that one, so treat it as a real
    // correctness fix (a process is never a thread) rather than a proven
    // independent cause.
    crate::sched::seed_thread_slots(task_slot, slot, crate::thread::NO_THREAD);
    Ok(())
}

/// Spawn flag bits (Akuma's own SPAWN ABI, arg6). Keep in sync with
/// `libakuma::SPAWN_FLAG_PTY` and glue's constant of the same name.
pub const SPAWN_FLAG_PTY: u64 = 1;
/// The child **is the console's process**: it gets the console's channel and its
/// shared line discipline instead of a session channel of its own, so its fd 0
/// reads the keyboard and its fd 1/2 draw on the screen. Keep in sync with
/// `libakuma::SPAWN_FLAG_CONSOLE`. Ignored by the AArch64 kernel, which has no
/// console input device to hand over.
const SPAWN_FLAG_CONSOLE: u64 = 2;

/// `spawn(path, argv, envp, stdin, stdin_len, flags)` — Akuma's own syscall 301.
///
/// Returns `pid | (stdout_fd << 32)` on success, or a negative errno.
///
/// # `SPAWN_FLAG_PTY`
///
/// Set by `libakuma::spawn_pty`, which `sshd` calls for a client's `pty-req`.
/// The doc here said the bit was "accepted and currently ignored"; it was
/// neither — the dispatch arm passed five arguments and the flags word is the
/// sixth, so it never arrived.
///
/// What it buys now is the **line discipline's identity**, not (yet) its data
/// path. The child gets a `ProcessChannel` marked `is_terminal()` and a fresh
/// `TerminalState` whose `foreground_pgid` is its own pid — which is exactly
/// what `akuma_exec::process::write_to_process_stdin`'s ISIG branch reads, so a
/// `^C` arriving on `/proc/<pid>/fd/0` is consumed by the terminal and raised as
/// `SIGINT` on the session's process group instead of being handed to the shell
/// as a `0x03` byte. The child's stdin **stays a pipe** ([`crate::fd::bind_stdio`]),
/// and the bridge's bytes reach it through that same shared sink, which since
/// 2026-09-11 delivers to a target whose fd 0 is a `PipeRead`.
///
/// What it does **not** buy, and the reason is worth stating rather than
/// discovering: cooked input and echo. Those come from glue's `Stdin` read arm,
/// which is reached only when fd 0 is a `FileDescriptor::Stdin`, and its echo
/// goes to the *channel's stdout FIFO* — which on this target nothing drains,
/// because a spawned child's stdout is a pipe and `sshd` reads that. Moving fd 0
/// onto the channel therefore has to move fd 1/2 with it, which is a different
/// piece (see the `ioctl` header in `crate::fd`).
pub fn sys_spawn(
    path_ptr: u64,
    argv_ptr: u64,
    envp_ptr: u64,
    stdin_ptr: u64,
    stdin_len: u64,
    flags: u64,
) -> u64 {
    use crate::fd::errno;

    let pty = (flags & SPAWN_FLAG_PTY) != 0;
    let console = (flags & SPAWN_FLAG_CONSOLE) != 0;

    // **Who may ask for the console.** A console child reads the keyboard, so
    // this is a grab on an input device, and the answer is the same one
    // `console_attached` gives for a process: only something already attached to
    // the console may hand it on. `init` and `herd` are; an `sshd` session's
    // shell carries its own channel and is not, so a login cannot take the
    // keyboard from the TV by asking.
    if console {
        let attached = akuma_exec::process::current_process_shared()
            .and_then(|p| p.channel.clone())
            .is_some_and(|ch| crate::console::is_console_channel(&ch));
        if !attached {
            return errno::EPERM;
        }
    }

    let Some(path_bytes) = user_cstr(path_ptr, 256) else {
        return errno::EFAULT;
    };
    let Ok(path) = core::str::from_utf8(&path_bytes) else {
        return errno::EINVAL;
    };

    // Same helper as `sys_execve`: symlink-resolved, chunked, fallible — so a
    // spawn of a link, or of any binary over 16 MiB, loads instead of failing
    // as ENOENT. The VFS error is passed through, not flattened.
    // Cwd-relative, exactly as `sys_execve` is — a spawn of `./prog` from a
    // shell means the shell's directory.
    let path = process_relative_path(path);
    let path = path.as_str();
    let head = match crate::fs::read_image_head(path) {
        Ok(head) => head,
        Err(e) => return akuma_syscalls_glue::fs::fs_error_to_errno(e),
    };

    // argv[0] defaults to the path if the caller passed none.
    let mut budget = exec_arg_budget();
    let argv_owned = {
        let Some(mut v) = user_strv(argv_ptr, loader::MAX_ARGV, &mut budget) else {
            return crate::fd::errno::E2BIG;
        };
        if v.is_empty() {
            v.push(path_bytes.clone());
        }
        v
    };
    // The environment, which this arm named `_envp` and threw away until
    // `AMD64_SSH_TERM_SIZE_NOT_PASSED.md` break 8: **no** environment reached
    // any spawned child on this target, so anything reading `TERM`, `PATH`,
    // `HOME` or `TZ` got nothing. `sys_execve` has always honoured it through
    // the same loader entry point; this is that call, with the same caps.
    let Some(envp_owned) = user_strv(envp_ptr, loader::MAX_ENVP, &mut budget) else {
        return crate::fd::errno::E2BIG;
    };
    let argv_refs: alloc::vec::Vec<&[u8]> =
        argv_owned.iter().map(alloc::vec::Vec::as_slice).collect();
    let envp_refs: alloc::vec::Vec<&[u8]> =
        envp_owned.iter().map(alloc::vec::Vec::as_slice).collect();
    // Kept for `/proc/<pid>/cmdline`: the loader writes argv onto the child's
    // stack and this vector is dropped, so this is the last chance to record it.
    let spawn_cmdline = flatten_cmdline(argv_refs.iter().copied());
    let spawner_pid = current_pid();

    // Same collect-first as `sys_fork`'s search, and for the same reason: the
    // reaper frees the process, this frees the row.
    sweep_reaped_spawn_rows();
    // A free process slot in the spawn range. The `SPAWN` row is the oracle
    // since 5b slice 4 — it was `PROCS`, and `fork` consulted both, which is
    // the divergence that made this function and that one disagree about how
    // full the machine was.
    let slot = SPAWN.first_free_slot();
    let Some(slot) = slot else {
        return errno::ENOMEM;
    };

    let (child, img) = match Image::from_elf_path_argv_envp(&head.resolved, &argv_refs, &envp_refs) {
        Ok(p) => p,
        Err(e) => {
            serial::puts("  [spawn] load failed: ");
            serial::puts(e);
            serial::puts("\n");
            // Same misattribution `sys_execve` carried: every rejection printed
            // as `Out of memory`. Same honest mapping.
            return exec_load_errno(e);
        }
    };

    let root = child.space.ttbr0();

    // **The session channel — this child's whole stdio, and nothing else.**
    //
    // One `ProcessChannel` serving all three jobs, which is verbatim what an
    // AArch64 spawn does (`akuma_exec::process::spawn_process_with_channel_ext`
    // plus glue's `sys_spawn`): the child's I/O (`Process::channel`, below), its
    // exit status (`register_channel(task_slot, ..)`), and the caller's handle
    // on it (`register_child_channel` + the `ChildStdout(pid)` descriptor
    // returned at the bottom).
    //
    // Until 2026-09-11 this was **two pipes** — `bind_stdio` put the stdin
    // pipe's read end at fd 0 and the stdout pipe's write end at fd 1 and 2, and
    // the caller got a `PipeRead` on the other end. That was a second
    // implementation of session stdio, living beside the shared one because
    // `sys_spawn` here predates it, and it is what kept an `ssh` session's shell
    // from having a line discipline at all: glue's `Stdin` read arm serves
    // cooked input and writes its **echo** to this channel's *stdout* FIFO, so a
    // session split across a channel and a pipe has its echo land where nothing
    // reads. There is no drain to add — there is a parallel implementation to
    // delete, which is what this is.
    //
    // **Except for a console child, which has none** — `channel` is `None` and
    // `register_exec_process` reads that, together with fd 0 being `Stdin`, as
    // "this is the console's process" and hands it `console::channel()` and the
    // shared `TerminalState`. The exit channel it registers for `wait4` is then
    // a fresh one that nothing writes to, so the `ChildStdout` the caller gets
    // back reads EOF at exit and nothing before it — the right shape for a
    // supervisor, whose child's output is on the screen, not in a pipe.
    let channel = (!console).then(|| {
        let ch = alloc::sync::Arc::new(akuma_exec::process::ProcessChannel::new());
        // `is_terminal()` is what glue's `Stdin` arm selects cooked input on, what
        // gates `/dev/tty`, and what `write_to_process_stdin`'s ISIG branch reads —
        // so `SPAWN_FLAG_PTY` decides whether `^C` is a signal or a byte. A plain
        // `spawn` (herd's services, the boot suite's `run_sh_capture`) is a pipe and
        // must not have its stream cooked.
        ch.set_terminal(pty);
        ch
    });

    // Seed the child's stdin, if the caller supplied any (`spawn_with_stdin`).
    //
    // **Closed straight after**, as the AArch64 spawn does: a seed is the whole
    // input, and a child that reads past it must see EOF rather than block on a
    // writer that will never come. `sshd` passes no seed for either an
    // interactive shell or an `exec` channel — it feeds them through
    // `/proc/<pid>/fd/0` and ends them with `close_child_stdin` — so this is the
    // in-kernel callers' path only.
    if let Some(channel) = &channel
        && stdin_ptr != 0
        && stdin_len != 0
        && let Some(seed) = crate::fd::copy_in(stdin_ptr, stdin_len.min(64 * 1024))
    {
        channel.write_stdin(&seed);
        channel.close_stdin();
    }

    // fd 0/1/2 = `Stdin`/`Stdout`/`Stderr`, served from the channel by glue's
    // own arms. `with_stdio()`, the same table every other registered process
    // gets — there is nothing spawn-specific left to bind.
    let child_fds = alloc::sync::Arc::new(akuma_exec::process::SharedFdTable::with_stdio());

    let Some(task_slot) = spawn_process_task(slot, root) else {
        // `child` drops here: the image is freed by the same destructor that
        // would have freed it out of `PROCS`. `child_fds` drops unregistered and
        // holds nothing refcounted — the stdio triple names no resource — so
        // there is nothing to unwind, where the pipe version had to release
        // three pipe-end references in a stated order.
        return errno::ENOMEM;
    };

    let pid = alloc_pid();
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
        // A spawned child gets a **fresh** terminal state, deliberately: that
        // is the state `sshd` then writes the client's window size into, and
        // two concurrent sessions must not share one. Same rule as the AArch64
        // `pty` spawn (`akuma-exec`'s `spawn.rs`).
        None,
        channel,
    );

    // **The session's foreground process group.**
    //
    // `TerminalState::default()` seeds `foreground_pgid = 1`, which is `init`'s
    // — so without this a `^C` on an ssh session would broadcast `SIGINT` at
    // `init`'s group rather than at the session's. The AArch64 spawn sets the
    // same field, for the same reason and unconditionally
    // (`spawn_process_with_channel_ext`); a spawn's terminal state is fresh
    // (the `None` above), so there is no caller's group to clobber.
    //
    // `kill_foreground_group` excludes the **leader**, so this names the shell and
    // reaches everything the shell forks — which is the job that should die on
    // `^C`, and not the shell running it.
    if let Some(proc) = akuma_exec::process::lookup_process_shared(pid) {
        proc.terminal_state.lock().set_session_leader(pid);
    }

    SPAWN.set(slot - SPAWN_SLOT_BASE, Spawn { pid, exec_slot: task_slot });

    // A quiet boot's splash hands the screen to the shell **before** the child can
    // be scheduled — it is cleared here, so the first prompt lands on a clean
    // console, not under an animation.
    if console {
        crate::splash::end_for_console();
    }

    // Last, as in `sys_fork`: identity and stdio in place before anything can
    // schedule the child.
    crate::sched::publish_task(task_slot);

    // The caller's handle on the child's output: a `ChildStdout(pid)`, which
    // glue's read arm resolves through `get_child_channel(pid)` to the very
    // channel above — blocking, `O_NONBLOCK`-aware, `EINTR`-able, and reporting
    // EOF from `has_exited()` rather than from a writer count. Verbatim what
    // glue's own `sys_spawn` hands back on the other kernel.
    //
    // It replaced a `PipeRead` on the stdout pipe, and two things came with the
    // swap for free: `TIOCSWINSZ` aimed at the child now resolves through glue's
    // `ChildStdout(pid)` arm (this file's `child_pipe_set_winsize` shim existed
    // only because a `PipeRead` does not name a child), and `close(2)` drops the
    // child-channel registration through glue's own arm.
    // The console shell exists, so the framebuffer is its now: from here the TV
    // shows the shell and not the kernel's diagnostics (`serial::FB_QUIET`).
    if console {
        serial::set_fb_quiet(true);
    }
    let stdout_fd = crate::fd::install_child_stdout(pid);
    if crate::fd::errno::is_err(stdout_fd) {
        // The child is already running; it will just write into a channel nobody
        // reads. Report the failure — `sshd` drops the session.
        return errno::EMFILE;
    }

    u64::from(pid) | (stdout_fd << 32)
}

// **`WAIT4_PARKED` is gone** (2026-09-10). It was a bitmap of every task parked
// inside this file's own hand-written `wait4` loop, woken wholesale on any
// child's exit — "a handful of spurious wakes at most", because the set carried
// no parent link and could not tell whose child had died.
//
// It is `ProcessChannel`'s poller list now, which does carry one: a waiter
// registers on the channel of the child (or children) it is actually waiting
// for, and `publish_child_exit` wakes exactly those. The wake path did not move
// so much as become answerable — see [`spawn_record_exit`] and the `Wait4` arm.

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
    let dying = SPAWN.get(proc_slot - SPAWN_SLOT_BASE).map_or(0, |s| s.pid);
    // **Is the row's pid the process that is actually exiting?** Everything
    // below marks `dying` exited and raises SIGCHLD on its parent. A row left
    // behind by an earlier occupant of this slot would publish the death of a
    // process that is still running — its parent's `wait4` reaps it, and an
    // interactive shell then returns to its prompt and starts reading the
    // terminal out from under its still-running foreground job. That is the
    // shape of Claude Code's `/login` input loss (2026-10-17: ash back in its
    // prompt loop while `claude` ran on); this line says whether it is this.
    if dying != 0 {
        let own = akuma_exec::process::read_current_pid().unwrap_or(0);
        let tgid = current_pid();
        if dying != own && dying != tgid {
            akuma_primitives::safe_print!(160,
                "[exit] slot {} row names pid {} but the exiting task is pid {} (tgid {}): stale SPAWN row\n",
                proc_slot, dying, own, tgid);
        }
    }

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
            // And the *reported* state. Nothing on this target read
            // `Process::state` to decide anything, so it was never set here,
            // and `/proc/<pid>/status` said `R (running)` for every exited,
            // unreaped process: a zombie whose parent never waited looked
            // exactly like "a shell hung in its own exit" (2026-10-05,
            // `userspace/forktest/c_stress/lifeprobe.c`). Only `/proc`, `ps`
            // and the load counters read it.
            p.state.store(akuma_exec::process::ProcessState::Zombie(status));
        });
        // Reparent this process's children onto init, the way Linux does at
        // exit — in BOTH views of parenthood. This used to rewrite only the
        // table row's `parent_pid`, while `wait4(-1)` / `has_children` /
        // `is_child_of_group` read the child-channel registry, which kept the
        // dead parent: init's `wait4(-1, WNOHANG)` answered `ECHILD`, and every
        // orphan on this target was a zombie until reboot (2026-09-23,
        // `docs/archive/AKUMA_AMD64_TRAP_ENTRY_DIRECTION_FLAG.md`).
        let _ = akuma_exec::process::reparent_children_to(dying, 1);
    }
    // Outside the `unsafe` block and after the status is recorded, both
    // deliberately: a woken parent re-runs its wait immediately, and it must
    // find `exited` already set or it parks again for nothing.
    //
    // `publish_child_exit` is the shared publisher, and it does in one call
    // what this function's own wake could not do at all: mark the child's
    // channel exited (waking the pollers registered on *that* child, not every
    // parked task) and **then** raise SIGCHLD on the parent, in that order,
    // because a shell's SIGCHLD handler re-polls with `WNOHANG` immediately and
    // must find the zombie. It publishes at most once per death, so a child
    // that exits and is then torn down does not raise two.
    if dying != 0 {
        akuma_exec::process::publish_child_exit(dying, status);
    }
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

#[must_use]
pub fn current_proc_slot() -> usize {
    // Under the BKL; the running task's slot, or `usize::MAX` with none.
    crate::sched::read_current_uctx(|u| u.proc_slot).unwrap_or(usize::MAX)
}

/// `close_child_stdin(pid)` — Akuma's syscall 326. `sshd` calls it when the
/// client sends EOF on the channel, so the shell sees end-of-input.
///
/// **Glue's arm**, since the child's stdin became a `ProcessChannel`
/// (2026-09-11). It was a `pipe::close_write` on the spawn row's own write
/// reference, found through a `stdin_pipe_for_pid` helper that scanned the spawn
/// table — the row being consulted rather than the child's fd table because
/// `sshd` was asking about the end **it** wrote, which no descriptor named.
/// There is no such end now: `close_process_stdin` marks the channel's stdin
/// closed and wakes whatever is parked in a read of it, which is the same job
/// done by the object that owns it.
pub fn sys_close_child_stdin(pid: u64) -> u64 {
    akuma_syscalls_glue::proc::sys_close_child_stdin(pid as u32)
}

/// Is `p` a child of `me` for `waitpid`'s purposes?
///
/// `parent_pid` is the link every process this target builds itself carries. A
/// process created by `spawn_ext` — `box run`/`open`/`use`, herd's services —
/// is built by `akuma-exec`, which records the caller as `spawner_pid` and
/// leaves `parent_pid` at 0 (AArch64 waits through child channels and never
/// reads it). Matching on `parent_pid` alone made every such child invisible to
/// its spawner's `waitpid`: `ECHILD` forever, and `box use -i` / `box run`
/// polling a finished container until the session was killed.
fn is_child_of(p: &akuma_exec::process::Process, me: u32) -> bool {
    p.parent_pid == me || (p.parent_pid == 0 && p.spawner_pid == Some(me))
}

/// `waitpid(pid, status_ptr, options)` — Akuma's syscall 303.
///
/// Non-blocking regardless of `options`: `sshd`'s bridge polls it every tick and
/// must keep draining the child's stdout while it waits. Returns `pid` and
/// writes the wait status (`encode_wait_status`: `code << 8`, or `WTERMSIG` for
/// a signal death) once the child has exited, `0`
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
        (is_child_of(p, me) && (any || p.pid == want)).then_some(())
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
        (is_child_of(p, me) && (any || p.pid == want) && p.exited.load(Ordering::Acquire))
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
    // `slot_off` came from a live row and nothing yields between.
    let exec_slot = SPAWN.get(slot_off).unwrap().exec_slot;

    // **Nothing to release here any more.** This used to drop the spawn's own
    // writer reference on the child's stdin pipe — the one no descriptor named,
    // so nothing else would, and a reap that skipped it leaked a pipe per spawn
    // against a 64-pipe ceiling. A spawned child's stdio is a `ProcessChannel`
    // now; the last `Arc` out drops it, and a reap has no bookkeeping to do.

    if status_ptr != 0 {
        // Glue's encoder, the one `wait4` uses: a negative code is a signal
        // death (`-(sig)`) and must read back as `WIFSIGNALED`. This used to be
        // `(code & 0xff) << 8` for everything, which reported a `SIGTERM` death
        // as a clean `exit(241)` — to `sshd` (which then sent `exit-status 241`
        // rather than `exit-signal TERM`) and to herd, whose `signaled()` never
        // fired. That is what hid the stale `GROUP_EXIT_STATUS` kill
        // (`spawn_process_task`) behind a meaningless-looking exit code.
        let raw = akuma_syscalls_glue::proc::encode_wait_status(code).cast_signed();
        // A bad `status` pointer loses the status, not the reap: the child is
        // already gone and `wait4` reporting its pid is the useful half.
        let _ = crate::uaccess::write_val::<i32>(status_ptr, raw);
    }

    // The child task is Finished — it called `sched::finish()` in `run_process`
    // after recording its exit.
    SPAWN.clear(slot_off);
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
/// Free every `SPAWN` row whose process has been reaped, and drop the one pipe
/// end nothing else holds.
///
/// # Why a sweep and not a reap hook
///
/// Since 2026-09-10 the reaper is `akuma-syscalls-glue`'s `wait4`: it
/// unregisters the process and reaps the child channel, and it knows nothing
/// about this target's `SPAWN` table — nor should it, since the two things left
/// in a row are a scheduler task slot and a `crate::pipe` id, neither of which
/// exists on the other kernel.
///
/// A hook was the obvious alternative and is the wrong shape here. The reap is
/// not the only way a row can become stale — a reparented orphan collected by
/// init, a `wait4` that returned `EFAULT` after unregistering, a future second
/// waiter — and a hook has to be called from every one of them, correctly,
/// forever. "The process this row names is no longer registered" is checkable
/// and true in all of them, so the sweep asks the question instead of trusting
/// a call site to answer it. It runs after each `wait4` and again when
/// `sys_spawn` looks for a free slot, which are the only two moments the answer
/// matters.
///
/// **`pipe::close_write`, not `pipe::free`**, for the reason
/// [`Spawn::stdin_pipe`]'s doc gives: a reader reference on that pipe can
/// It released the spawn's stdin-pipe writer reference too, until the pipes
/// themselves went (2026-09-11). A `ProcessChannel` needs no such hand: the last
/// `Arc` out drops it.
pub fn sweep_reaped_spawn_rows() {
    for off in 0..SPAWN_SLOTS {
        // Under the BKL; nothing yields between the read and the write below.
        let Some(Spawn { pid, exec_slot: slot }) = SPAWN.get(off) else {
            continue;
        };
        if akuma_exec::process::lookup_process_shared(pid).is_some() {
            continue;
        }
        // The child's task reached `sched::finish()` before its process could
        // be unregistered, so nothing is running on `slot`.
        SPAWN.clear(off);
        // The identity map entry this row's spawn published, removed only if it
        // still names this pid — task slots are recycled, and stripping a live
        // process's identity would degrade every syscall it makes until its own
        // insert re-stamped.
        if akuma_exec::process::pid_for_thread(slot) == Some(pid) {
            akuma_exec::process::thread_pid_map_remove(slot);
        }
    }
}

pub fn spawn_row_of(pid: u32) -> Option<usize> {
    SPAWN.position_of(pid)
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
    // Resolved first, like `execve`'s `read_image_head`: a usr-merged root has
    // `/bin -> usr/bin` and `sh -> bash`, and `read_file` follows neither.
    let Ok(image) = crate::fs::read_file(&crate::fs::resolve_symlinks(path)) else {
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
    register_exec_process(1, 0, task_slot, proc, init_image_top, path, &INIT_CMDLINE.lock().clone(), None, None, None);
    // **The console's producer**, started here and nowhere earlier.
    //
    // After the registration, so the channel it fills already belongs to a
    // process; before `publish_task`, so init cannot reach its first `read(0)`
    // with nothing draining the UART. Not during the self-test suite: the pump
    // owns `input::getb` destructively once it runs, and the suite's own
    // console checks (`fd::console_nonblock_test`) are written against an idle
    // line — starting it there would add a second reader of the hardware for no
    // gain, since no test types.
    // The screen's real size into the console's terminal state (so `stty size`
    // and every program asking `TIOCGWINSZ` see the framebuffer grid, not a
    // default 24x80), and the one-line geometry summary at the end of the boot
    // log. No framebuffer (PVH, serial) leaves the 24x80 default alone.
    //
    // **`/etc/console.conf` first**: a machine's own screen setup — margin, and how
    // much of the screen the console uses (`cols = 50%` for the left half) — is read
    // from its filesystem here, so the size reported below is the one in force.
    // Absent or unreadable is the normal case and changes nothing.
    //
    // During a quiet boot the splash is on the screen and applying a margin would
    // clear it, so the text is held until the splash ends (`splash::end_for_console`).
    // An init that will not spawn a console shell (`herd` does; a bare shell is the
    // console itself) has nothing to wait for, so the splash ends here.
    if !path.ends_with("herd") {
        crate::splash::end_for_init();
    }
    if let Ok(conf) = crate::fs::read_file("/etc/console.conf")
        && let Ok(text) = core::str::from_utf8(&conf)
        && !crate::splash::defer_config(text)
    {
        let _ = crate::multiboot2::fb_apply_config(text);
    }
    if let Some((rows, cols)) = crate::multiboot2::fb_grid() {
        crate::console::set_size(rows, cols);
        crate::multiboot2::fb_summary();
    }
    // The USB keyboard, if the xHCI controller is up (a USB root disk, or the
    // `usb` flag): enumerate it before the pump starts reading input, so the
    // first lap already has a keyboard. A failure costs the keyboard only — the
    // log says which step, and the serial console still works.
    if crate::xhci::is_initialized()
        && let Err(e) = crate::xhci::init_keyboard()
    {
        serial::puts("  [init] no USB keyboard: ");
        serial::puts(e);
        serial::puts("\n");
    }
    if !crate::console::spawn_pump() {
        serial::puts("  [init] WARNING: no task slot for the console pump — the console will not deliver input\n");
    }
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
    //
    // **It parks between checks; it used to spin.** This loop was a bare
    // `yield_now()`, and it is what kept this target's vCPU at 100% for the
    // whole life of every boot — measured 2026-09-13 on the Firecracker rig: an
    // idle guest, nothing running but `sshd`, burned 102% of a host core, and
    // the tick-sampled profile (`sched::tick_profile`, written to answer this)
    // read `idle=0 t0=4818` — every sample on slot 0, none on the idle thread.
    // `sched::idle_loop` can only reach its `hlt` when nothing is runnable, and
    // this thread was always runnable.
    //
    // Two poll loops were parked first on the theory that they were the cause
    // (`net::netpoll_daemon`, `console::pump_daemon`); both were, in their own
    // right, and neither moved the number. The profiler did in one boot what
    // three rounds of reading code did not, which is the lesson worth keeping.
    //
    // Why a deadline and not an event: what this waits for is "every user task
    // finished", which no single site publishes — there is no one place to hang
    // a wake on. One tick of latency on noticing that init exited costs
    // nothing; a spin costs the core a compile is trying to use.
    //
    // Why the `timer_running` guard: `uptime_us` advances only on the tick, and
    // `lib.rs` arms the timer only when there is a network. Parking against a
    // clock that does not move would hang a no-network boot forever — the same
    // condition `console::pump_daemon` carries, for the same reason.
    while !crate::sched::all_user_tasks_finished() {
        if crate::lapic::timer_running() {
            crate::sched::block_until_deadline(
                crate::net::uptime_us() + u64::from(crate::lapic::US_PER_TICK_TARGET),
            );
        } else {
            crate::sched::yield_now();
        }
    }
    serial::puts("\n-- init exited --\n");
    true
}




