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
/// Bytes accepted by `write`, across all processes.
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

    /* Shift the Linux argument registers into System V positions:
     *   Linux:    nr=rax  a1=rdi  a2=rsi  a3=rdx
     *   System V: 1 =rdi  2 =rsi  3 =rdx  4 =rcx
     * so (nr, a1, a2, a3) -> (rdi, rsi, rdx, rcx).
     *
     * Assigned right-to-left, or each move clobbers the next one's source.
     * `rcx` is free to use as the fourth argument even though `syscall` put the
     * user return address there — it was pushed above and is restored below. */
    mov rcx, rdx
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
    /* rdi = user rip, rsi = user rsp. Returns when userspace exits.
     *
     * Clear LEAVE_RING3 first. Its lifetime is exactly one excursion, and it is
     * set by the handler on the way out — so a *second* entry with the flag
     * still set returns immediately after the first syscall, whatever that
     * syscall was. That is not hypothetical: it made a second process report its
     * write() length (55) as its exit status instead of 0x0B, with both
     * processes otherwise behaving correctly. */
    mov qword ptr [rip + LEAVE_RING3], 0

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
            // SAFETY: single core, interrupts off inside a syscall; read only by
            // `syscall_entry` on the way out. Bound to a local first — writing
            // `*(&raw mut X)` inline trips `clippy::deref_addrof`, whose
            // suggested fix reintroduces the `static_mut_refs` violation.
            unsafe {
                let flag = &raw mut LEAVE_RING3;
                *flag = 1;
            }
            a1
        }
        Syscall::Getpid => 1,
        Syscall::SchedYield => 0,
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
/// Built rather than written as a byte literal so the message offset is
/// *computed*. A hand-assembled blob with a hardcoded operand is exactly the
/// kind of thing that stays correct until someone edits the message by one
/// character.
///
/// ```text
///   mov rax, <write>        ; number from Syscall::Write.to_x86_64()
///   mov rdi, 1              ; fd = stdout
///   movabs rsi, <msg va>    ; patched once the layout is known
///   mov rdx, <len>
///   syscall
///   mov rax, <exit_group>
///   mov rdi, <status>
///   syscall
///   jmp $                   ; a guard, not a fallthrough
///   <message bytes>
/// ```
fn build_user_program(out: &mut [u8], base_va: u64, msg: &[u8], status: u32) -> usize {
    let mut n = 0;
    let mut emit = |bytes: &[u8], n: &mut usize| {
        out[*n..*n + bytes.len()].copy_from_slice(bytes);
        *n += bytes.len();
    };

    // mov r64, imm32 (sign-extended). Opcode byte differs per destination.
    let mov_imm = |modrm: u8, v: u32| {
        let b = v.to_le_bytes();
        [0x48, 0xC7, modrm, b[0], b[1], b[2], b[3]]
    };
    const RAX: u8 = 0xC0;
    const RDI: u8 = 0xC7;
    const RDX: u8 = 0xC2;

    emit(&mov_imm(RAX, Syscall::Write.to_x86_64() as u32), &mut n);
    emit(&mov_imm(RDI, 1), &mut n);

    // movabs rsi, imm64 — the message address, patched below.
    let movabs_at = n;
    emit(&[0x48, 0xBE, 0, 0, 0, 0, 0, 0, 0, 0], &mut n);

    emit(&mov_imm(RDX, msg.len() as u32), &mut n);
    emit(&[0x0F, 0x05], &mut n); // syscall

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
struct Process {
    space: paging::AddressSpace,
    code: usize,
    stack: usize,
}

impl Process {
    /// Build a process that prints `msg` and exits with `status`.
    fn new(msg: &[u8], status: u32) -> Option<Self> {
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
            build_user_program(page, USER_CODE_VA as u64, msg, status);
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

    /// Switch to this process's address space, run it, and switch back.
    fn run(&self) -> u64 {
        let kernel_root = paging::active_root();
        // SAFETY: the space shares the kernel's PDPT slot 0 (identity-mapped
        // first 1 GiB: image, stacks, heap, PMM pool) and slot 3 (the LAPIC), so
        // every page this kernel executes from or touches while the process runs
        // is mapped in it. That is exactly the obligation `activate` states.
        unsafe { paging::activate(self.space.root()) };

        let user_rsp = (USER_STACK_VA + 4096 - 16) as u64;
        // SAFETY: both pages are mapped with user permissions in the now-active
        // address space, and the program ends in exit_group, which returns here.
        let status = unsafe { enter_user_mode(USER_CODE_VA as u64, user_rsp) };

        // SAFETY: returning to the address space we came from, which is still
        // intact — nothing freed it while the process ran.
        unsafe { paging::activate(kernel_root) };
        status
    }

    fn free(self) {
        akuma_pmm::free_page(self.code, 0);
        akuma_pmm::free_page(self.stack, 0);
        self.space.free();
    }
}

/// Run two processes in separate address spaces and prove they are isolated.
pub fn smoke_test() {
    serial::puts("  test: processes — userspace output follows\n");

    let free_before = akuma_pmm::free_count();

    let (Some(a), Some(b)) = (
        Process::new(b"    [ring3 A] first process, own address space\n", 0x0A),
        Process::new(b"    [ring3 B] second process, same VA, different frame\n", 0x0B),
    ) else {
        serial::puts("  [FAIL] could not build processes\n");
        return;
    };

    // The whole point, checked before either runs: the same virtual address
    // resolves to different physical frames, and to nothing at all in the
    // kernel's own space.
    let pa_a = a.space.translate(USER_CODE_VA);
    let pa_b = b.space.translate(USER_CODE_VA);
    let pa_k = paging::translate(USER_CODE_VA);

    let status_a = a.run();
    let status_b = b.run();

    let calls = CALLS.load(Ordering::Relaxed);
    let isolated = pa_a.is_some() && pa_b.is_some() && pa_a != pa_b && pa_k.is_none();
    let ran = status_a == 0x0A && status_b == 0x0B && calls == 4;

    serial::puts("  test: processes 0x");
    serial::put_hex(pa_a.unwrap_or(0));
    serial::puts(" vs 0x");
    serial::put_hex(pa_b.unwrap_or(0));
    serial::puts(" at the same VA, exits 0x");
    serial::put_hex(status_a);
    serial::puts("/0x");
    serial::put_hex(status_b);
    serial::puts(if isolated && ran { "   [OK]\n" } else { "   [FAIL]\n" });

    a.free();
    b.free();

    // Address spaces are page tables; leaking them leaks frames silently.
    let free_after = akuma_pmm::free_count();
    serial::puts("  test: address-space teardown frames ");
    serial::put_dec(free_before as u64);
    serial::puts(" -> ");
    serial::put_dec(free_after as u64);
    serial::puts(if free_before == free_after {
        "   [OK]\n"
    } else {
        "   [FAIL]\n"
    });
}
