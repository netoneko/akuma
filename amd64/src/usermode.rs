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
use crate::paging::{self, MemAttr, Prot};
use crate::serial;
use core::sync::atomic::{AtomicU64, Ordering};

const IA32_EFER: u32 = 0xC000_0080;
const IA32_STAR: u32 = 0xC000_0081;
const IA32_LSTAR: u32 = 0xC000_0082;
const IA32_FMASK: u32 = 0xC000_0084;

/// `EFER.SCE` — without it, `syscall` raises `#UD`.
const EFER_SCE: u64 = 1 << 0;

/// Where the test's user code and stack live. Above everything else the port
/// maps, so a mistake cannot silently land on an existing mapping.
const USER_CODE_VA: usize = 0x5000_0000;
const USER_STACK_VA: usize = 0x5001_0000;

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
const MAX_WRITE: u64 = 4096;

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
    sub rsp, 8                      /* System V wants rsp 16-aligned at `call` */

    /* Linux arg registers into System V positions:
     *   Linux:    nr=rax  a1=rdi  a2=rsi  a3=rdx
     *   System V: 1 =rdi  2 =rsi  3 =rdx  4 =rcx
     * Assigned right-to-left, or each move clobbers the next one's source.
     * `rcx` is free even though `syscall` put the user return address there —
     * it was pushed above and is restored below. */
    mov rcx, rdx
    mov rdx, rsi
    mov rsi, rdi
    mov rdi, rax
    call syscall_handler            /* result in rax */

    add rsp, 8
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
extern "C" fn syscall_handler(nr: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    CALLS.fetch_add(1, Ordering::Relaxed);

    // Errno values are returned as negatives, the Linux convention.
    const ENOSYS: u64 = (-38i64) as u64;
    const EBADF: u64 = (-9i64) as u64;
    const EINVAL: u64 = (-22i64) as u64;

    let Some(call) = Syscall::from_x86_64(nr) else {
        return ENOSYS;
    };

    match call {
        Syscall::Write => sys_write(a1, a2, a3),
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
        Syscall::Close => EBADF,
        Syscall::Brk => EINVAL,
        _ => ENOSYS,
    }
}


/// `write(fd, buf, len)` — fd 1 and 2 go to the serial console.
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
fn build_user_program(out: &mut [u8], base_va: u64, msg: &[u8], rounds: u32, status: u32) -> usize {
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

    emit(&mov_imm(RAX, Syscall::SchedYield.to_x86_64() as u32), &mut n);
    emit(&[0x0F, 0x05], &mut n); // syscall

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

/// A process: its own address space plus the frames backing it.
pub struct Process {
    space: paging::AddressSpace,
    code: usize,
    stack: usize,
}

impl Process {
    /// Build a process that prints `msg` `rounds` times, yielding between each,
    /// then exits with `status`.
    fn new(msg: &[u8], rounds: u32, status: u32) -> Option<Self> {
        let space = paging::AddressSpace::new()?;
        let (Some(code), Some(stack)) = (akuma_pmm::alloc_page(), akuma_pmm::alloc_page()) else {
            space.free();
            return None;
        };

        // SAFETY: PMM frames are inside the identity map, so the physical
        // address is a valid pointer for staging the program *before* the
        // address space that will hold it is ever activated.
        unsafe {
            core::ptr::write_bytes(code as *mut u8, 0, 4096);
            core::ptr::write_bytes(stack as *mut u8, 0, 4096);
            let page = core::slice::from_raw_parts_mut(code as *mut u8, 4096);
            build_user_program(page, USER_CODE_VA as u64, msg, rounds, status);
        }

        if !space.map(USER_CODE_VA, code as u64, Prot::USER_RX, MemAttr::WriteBack)
            || !space.map(USER_STACK_VA, stack as u64, Prot::USER_RW, MemAttr::WriteBack)
        {
            akuma_pmm::free_page(code, 0);
            akuma_pmm::free_page(stack, 0);
            space.free();
            return None;
        }
        Some(Self { space, code, stack })
    }

    fn free(self) {
        akuma_pmm::free_page(self.code, 0);
        akuma_pmm::free_page(self.stack, 0);
        self.space.free();
    }
}

/// The two processes the test runs, reachable from their task entry points.
///
/// `extern "C" fn() -> !` takes no arguments, so a task entry cannot be handed
/// its process. A slot per process is the smallest thing that works on one core.
static mut PROCS: [Option<Process>; 2] = [None, None];

/// Enter ring 3 for process `idx`, then mark the task finished.
///
/// The scheduler has already installed this task's address space by the time
/// this runs — `spawn_in_space` recorded the root, and `yield_now` writes `CR3`
/// before switching stacks.
fn run_process(idx: usize) -> ! {
    // SAFETY: single core; each slot is written once before its task is spawned
    // and read only by that task.
    let space_ok = unsafe {
        let procs = &raw const PROCS;
        (*procs)[idx].is_some()
    };
    if space_ok {
        let user_rsp = (USER_STACK_VA + 4096 - 16) as u64;
        // SAFETY: both pages are mapped user-accessible in the address space the
        // scheduler installed for this task, and the program ends in exit_group.
        let status = unsafe { enter_user_mode(USER_CODE_VA as u64, user_rsp) };
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

/// Run two isolated processes concurrently and prove they interleave.
pub fn smoke_test(t: &mut Suite) {
    const ROUNDS: u32 = 3;
    const MSG_A: &[u8] = b"    [ring3 A] round\n";
    const MSG_B: &[u8] = b"    [ring3 B] round\n";

    let free_before = akuma_pmm::free_count();

    let (Some(a), Some(b)) = (
        Process::new(MSG_A, ROUNDS, 0x0A),
        Process::new(MSG_B, ROUNDS, 0x0B),
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
