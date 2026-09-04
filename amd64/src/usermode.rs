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
/// Pages of stack. Two, because the initial frame is under 200 bytes and
/// `hello` does not recurse; a real program needs a guard page and a growth
/// policy, and this kernel has neither.
const ELF_STACK_PAGES: usize = 2;

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
/// `[rax + 0]`, `[rax + 8]` and `[rax + 16]`. Reordering the fields silently
/// changes what that assembly reads.
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
}

impl UserCtx {
    #[must_use]
    pub const fn new() -> Self {
        Self { kernel_rsp: 0, user_rsp: 0, leave: 0 }
    }
}

/// The running task's [`UserCtx`]. Repointed by the scheduler on every switch.
#[unsafe(no_mangle)]
pub static mut CURRENT_UCTX: *mut UserCtx = core::ptr::null_mut();

/// How many syscalls arrived.
static CALLS: AtomicU64 = AtomicU64::new(0);
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
    /* rdi = user rip, rsi = user rsp. Returns when userspace exits. */
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
    sysretq

    .section .bss
    .align 16
SYSCALL_SCRATCH:
    .skip 8
"#
);

unsafe extern "C" {
    /// Drop to ring 3 at `rip` with stack `rsp`; returns when userspace calls
    /// syscall 0.
    ///
    /// # Safety
    /// `rip` must point at a page mapped user-executable and `rsp` at a page
    /// mapped user-writable, both in the address space that is live.
    fn enter_user_mode(rip: u64, rsp: u64) -> u64;
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

    use crate::fd::errno;

    let Some(call) = Syscall::from_x86_64(nr) else {
        return errno::ENOSYS;
    };

    match call {
        Syscall::Write => sys_write(a1, a2, a3),
        Syscall::Read => crate::fd::sys_read(a1, a2, a3),
        Syscall::Openat => crate::fd::sys_openat(a1, a2, a3, a4),
        Syscall::Close => crate::fd::sys_close(a1),
        Syscall::Lseek => crate::fd::sys_lseek(a1, a2, a3),
        Syscall::Fstat => crate::fd::sys_fstat(a1, a2),
        Syscall::Ioctl => crate::fd::sys_ioctl(a1, a2, a3),
        // `a5` is mmap's fd and is deliberately unused: only anonymous mappings
        // are supported, so a file-backed request must fail rather than quietly
        // return zeroed memory that the caller believes holds a file.
        Syscall::Mmap => crate::mm::sys_mmap(a1, a2, a3, a4, a5),
        Syscall::Munmap => crate::mm::sys_munmap(a1, a2),
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

    if fd != 1 && fd != 2 {
        return EBADF;
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
        let space = paging::AddressSpace::new().ok_or("no frame for a PML4")?;
        let mut frames = FrameSet::new();

        let built = loader::load(image, &space, &mut frames).and_then(|img| {
            loader::build_stack(
                &space,
                &mut frames,
                ELF_STACK_TOP,
                ELF_STACK_PAGES,
                b"hello",
                img.entry,
            )
            .map(|rsp| (img, rsp))
        });

        match built {
            Ok((img, stack)) => {
                let entry = img.entry;
                Ok((Self { space, frames, entry, stack }, img))
            }
            Err(e) => {
                frames.free_all();
                space.free();
                Err(e)
            }
        }
    }

    fn free(mut self) {
        self.frames.free_all();
        self.space.free();
    }
}

/// The processes the tests run, reachable from their task entry points.
///
/// `extern "C" fn() -> !` takes no arguments, so a task entry cannot be handed
/// its process. A slot per process is the smallest thing that works on one core.
static mut PROCS: [Option<Process>; 5] = [None, None, None, None, None];

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
        (*procs)[idx].as_ref().map(|p| (p.entry, p.stack))
    };
    if let Some((entry, stack)) = start {
        // SAFETY: both are addresses the loader (or `Process::new`) mapped
        // user-accessible in the address space the scheduler installed for this
        // task, and every program this kernel runs ends in exit_group.
        let status = unsafe { enter_user_mode(entry, stack) };
        EXIT_STATUS.store(status, Ordering::Relaxed);
    }
    crate::sched::finish();
}

extern "C" fn proc0_entry() -> ! {
    run_process(0);
}
extern "C" fn proc1_entry() -> ! {
    run_process(1);
}
extern "C" fn proc2_entry() -> ! {
    run_process(2);
}
extern "C" fn proc3_entry() -> ! {
    run_process(3);
}
extern "C" fn proc4_entry() -> ! {
    run_process(4);
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
