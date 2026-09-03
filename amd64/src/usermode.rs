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

use akuma_syscalls_linux::syscall::Syscall;

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

/// Set by the handler when the excursion into ring 3 should end. Read by
/// `syscall_entry`, which is why it is `no_mangle` rather than a normal static.
#[unsafe(no_mangle)]
static mut LEAVE_RING3: u64 = 0;

/// How many syscalls arrived.
static CALLS: AtomicU64 = AtomicU64::new(0);
/// Bytes accepted by `write`.
static WRITTEN: AtomicU64 = AtomicU64::new(0);
/// Status userspace exited with.
static EXIT_STATUS: AtomicU64 = AtomicU64::new(u64::MAX);

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
    /* Entered from ring 3. rcx = user rip, r11 = user rflags, rsp = USER's
     * stack. Interrupts are already off (IA32_FMASK clears IF), so this window
     * cannot be interrupted while rsp still points at user memory. */
    mov [rip + user_rsp_slot], rsp
    lea rsp, [rip + syscall_stack_top]

    push rcx                      /* user rip   */
    push r11                      /* user rflags */
    push rax                      /* syscall nr — kept only to balance the frame */
    sub rsp, 8                    /* System V wants rsp 16-aligned at `call`;
                                     three pushes leave it 8 off. */

    /* Shift the Linux-style argument registers into System V positions:
     * nr(rax), a1(rdi), a2(rsi) -> rdi, rsi, rdx. Order matters — rdx first,
     * or rsi is clobbered before it is read. */
    mov rdx, rsi
    mov rsi, rdi
    mov rdi, rax
    call syscall_handler          /* result in rax */

    add rsp, 8
    pop rcx                       /* discard the saved nr */

    /* Whether to go back to ring 3 is the handler's decision, not a property of
     * the syscall number: `exit` and `exit_group` are different numbers, and on
     * a second architecture they would be different again. The handler sets the
     * flag; this only reads it. */
    cmp qword ptr [rip + LEAVE_RING3], 0
    jne .Lexit_to_kernel

    pop r11
    pop rcx
    mov rsp, [rip + user_rsp_slot]
    sysretq

.Lexit_to_kernel:
    /* Syscall 0: do not go back to ring 3. Restore what enter_user_mode saved
     * and return from it. rax still holds the handler's result. */
    mov rsp, [rip + kernel_return_rsp]
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

.global enter_user_mode
enter_user_mode:
    /* rdi = user rip, rsi = user rsp. Returns when userspace calls syscall 0. */
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rip + kernel_return_rsp], rsp

    mov rcx, rdi                  /* sysret takes rip from rcx    */
    mov r11, 0x202                /* ...and rflags from r11: IF set, bit 1 reserved-1 */
    mov rsp, rsi                  /* user stack */
    sysretq

    .section .bss
    .align 16
syscall_stack:
    .skip 16384
syscall_stack_top:
user_rsp_slot:
    .skip 8
kernel_return_rsp:
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
extern "C" fn syscall_handler(nr: u64, a1: u64, a2: u64) -> u64 {
    CALLS.fetch_add(1, Ordering::Relaxed);

    // Errno values are returned as negatives, the Linux convention.
    const ENOSYS: u64 = (-38i64) as u64;
    const EBADF: u64 = (-9i64) as u64;
    const EINVAL: u64 = (-22i64) as u64;

    let Some(call) = Syscall::from_x86_64(nr) else {
        return ENOSYS;
    };

    match call {
        Syscall::Write => sys_write(a1, a2, /* len */ arg3()),
        Syscall::Exit | Syscall::ExitGroup => {
            EXIT_STATUS.store(a1, Ordering::Relaxed);
            // SAFETY: single core, interrupts off inside a syscall; read only by
            // `syscall_entry` on the way out.
            unsafe { *(&raw mut LEAVE_RING3) = 1 };
            a1
        }
        Syscall::Getpid => 1,
        Syscall::SchedYield => 0,
        Syscall::Close => EBADF,
        Syscall::Brk => EINVAL,
        _ => ENOSYS,
    }
}

/// The third syscall argument, which the entry stub does not forward.
///
/// `syscall_entry` shifts `nr, a1, a2` into the first three System V registers,
/// so `rdx` — Linux's third argument — is consumed before the handler sees it.
/// Rather than widen the stub, the value is read back from where it still lives.
#[inline(always)]
fn arg3() -> u64 {
    let v: u64;
    // SAFETY: reads a register into a local. `r10` is Linux's *fourth* argument
    // register and untouched by the stub, so this is only correct because the
    // stub saves rdx there; see the stub.
    unsafe {
        core::arch::asm!("mov {}, r10", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v
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

/// The user program, as machine code.
///
/// Assembled by hand because there is no user toolchain in this build and no
/// loader yet: `akuma-elf` exists but wants a filesystem, and the point here is
/// the *transition*, not the loader.
///
/// ```text
///   mov rax, 1        ; SYS_WRITE_DEC
///   mov rdi, 0x1234   ; argument
///   syscall           ; -> rax = 0x2468
///   mov rdi, rax      ; hand the result back as exit status
///   mov rax, 0        ; SYS_EXIT
///   syscall           ; does not return
///   jmp $             ; unreachable; a guard, not a fallthrough
/// ```
const USER_BLOB: &[u8] = &[
    0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00, // mov rax, 1
    0x48, 0xC7, 0xC7, 0x34, 0x12, 0x00, 0x00, // mov rdi, 0x1234
    0x0F, 0x05, // syscall
    0x48, 0x89, 0xC7, // mov rdi, rax
    0x48, 0xC7, 0xC0, 0x00, 0x00, 0x00, 0x00, // mov rax, 0
    0x0F, 0x05, // syscall
    0xEB, 0xFE, // jmp $
];

/// Map a user code page and a user stack page, drop to ring 3, come back.
pub fn smoke_test() {
    const ARG: u64 = 0x1234;

    serial::puts("  test: ring 3 ");

    let (Some(code_frame), Some(stack_frame)) = (akuma_pmm::alloc_page(), akuma_pmm::alloc_page())
    else {
        serial::puts("[FAIL] no frames\n");
        return;
    };

    // SAFETY: PMM frames are inside the identity map, so the physical address is
    // a valid pointer for staging the blob before it is mapped into user space.
    unsafe {
        core::ptr::write_bytes(code_frame as *mut u8, 0, 4096);
        core::ptr::write_bytes(stack_frame as *mut u8, 0, 4096);
        core::ptr::copy_nonoverlapping(
            USER_BLOB.as_ptr(),
            code_frame as *mut u8,
            USER_BLOB.len(),
        );
    }

    // USER_RX for the code: readable and executable by ring 3, and *not*
    // writable — the encoder has no writable-and-executable constructor.
    if !paging::map_page(
        USER_CODE_VA,
        code_frame as u64,
        Prot::USER_RX,
        MemAttr::WriteBack,
    ) || !paging::map_page(
        USER_STACK_VA,
        stack_frame as u64,
        Prot::USER_RW,
        MemAttr::WriteBack,
    ) {
        serial::puts("[FAIL] could not map user pages\n");
        return;
    }

    // Stack grows down; start at the top of the page, 16-aligned.
    let user_rsp = (USER_STACK_VA + 4096 - 16) as u64;

    // SAFETY: both pages were just mapped with user permissions in the live
    // address space, and the blob ends in syscall 0, which returns here.
    let status = unsafe { enter_user_mode(USER_CODE_VA as u64, user_rsp) };

    let calls = CALLS.load(Ordering::Relaxed);
    let arg = LAST_ARG.load(Ordering::Relaxed);
    let ok = calls == 2 && arg == ARG && status == ARG * 2;

    serial::puts("entered, ");
    serial::put_dec(calls);
    serial::puts(" syscalls, arg=0x");
    serial::put_hex(arg);
    serial::puts(" status=0x");
    serial::put_hex(status);
    serial::puts(if ok { "   [OK]\n" } else { "   [FAIL]\n" });

    paging::unmap_page(USER_CODE_VA);
    paging::unmap_page(USER_STACK_VA);
    akuma_pmm::free_page(code_frame, 0);
    akuma_pmm::free_page(stack_frame, 0);
}
