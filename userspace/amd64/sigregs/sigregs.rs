//! Does a signal handler's excursion give back **every** register it found?
//!
//! `usermode::sigregs_test` runs this from the boot suite. The question it
//! answers is the one the Go/crush startup crash turned on
//! (`docs/archive/AMD64_FORK_VS_THREADS_SEGV.md`): a signal delivered at a
//! LAPIC tick or at a fault interrupts an arbitrary instruction, so `rcx`,
//! `r11` and the SSE/AVX registers are all live — and the return used to be a
//! `sysretq`, which loads `rcx` from `rip` and `r11` from `rflags`, with no
//! FPU state saved at all. Go's `memmove` came back with `RCX = RIP`.
//!
//! The shape, raw (no libc, same conventions as `killprobe`):
//!
//! * a `fork`ed child `kill`s the parent with `SIGUSR1` every millisecond, so a
//!   signal is pending while the parent is **busy in ring 3** and is delivered
//!   at the next tick — the exact path, on one core or several;
//! * the parent spins through *windows*: one `asm!` block that loads sentinel
//!   values into `rcx`, `r11` and `xmm0`–`xmm15` (plus the upper halves of
//!   `ymm0`–`ymm3` when the machine has AVX), idles with all of them live, then
//!   compares every one against its sentinel and reports a mismatch mask;
//! * every eighth window also stores through address 0 with the sentinels
//!   live, which the `SIGSEGV` handler resumes past — the fault-delivery path
//!   shares the return with the tick path and had the same defect;
//! * both handlers deliberately clobber everything they can reach.
//!
//! Exit status: `0` when enough signals were taken with no mismatch; `1` on a
//! mismatch (the mask is written to fd 2 as hex: bits 0–15 `xmm0`–`xmm15`, 16
//! `rcx`, 17 `r11`, 18–21 the `ymm0`–`ymm3` upper halves); `2` `fork` failed;
//! `3` too few deliveries before the iteration cap; `4` `rt_sigaction` failed;
//! `5` too few faults resumed. Before the fix the first window a signal lands
//! in reports `0x10000` — `rcx`, holding the interrupted `rip`.

#![no_std]
#![no_main]

const SYS_WRITE: u64 = 1;
const SYS_RT_SIGACTION: u64 = 13;
const SYS_NANOSLEEP: u64 = 35;
const SYS_GETPID: u64 = 39;
const SYS_FORK: u64 = 57;
const SYS_WAIT4: u64 = 61;
const SYS_KILL: u64 = 62;
const SYS_EXIT_GROUP: u64 = 231;

const SIGUSR1: u64 = 10;
const SIGSEGV: u64 = 11;
const SIGKILL: u64 = 9;

const SA_SIGINFO: u64 = 0x4;
const SA_RESTORER: u64 = 0x0400_0000;
const SA_NODEFER: u64 = 0x4000_0000;

/// Signals the parent must take with everything intact.
const WANT_DELIVERIES: u64 = 48;
/// Faults the parent must resume from — one every `FAULT_EVERY` windows.
const WANT_FAULTS: u64 = 8;
const FAULT_EVERY: u64 = 8;
/// Windows before giving up on the deliveries. One window is `IDLE_SPINS`
/// `pause`s plus a few dozen instructions.
const MAX_WINDOWS: u64 = 60_000;
const IDLE_SPINS: u64 = 8_000;
/// Signals the child sends before exiting, and how long it sleeps between.
const CHILD_SENDS: u64 = 4_000;
const CHILD_SLEEP_NS: u64 = 1_000_000;

/// `struct sigaction`, the kernel's x86_64 layout.
#[repr(C)]
struct KSigaction {
    handler: u64,
    flags: u64,
    restorer: u64,
    mask: u64,
}

/// `struct timespec`.
#[repr(C)]
struct Timespec {
    sec: u64,
    nsec: u64,
}

/// The sentinel block the window loads from and compares against: 16 lanes of
/// 16 bytes for `xmm0`–`xmm15` at 0, then four 32-byte `ymm0`–`ymm3` images at
/// 256 whose low halves equal lanes 0–3. 32-aligned: `pcmpeqd xmm, m128` and
/// `vmovdqu ymm, m256` want it (the former insists).
#[repr(C, align(32))]
struct Sentinels([u8; 384]);
static mut SENTINELS: Sentinels = Sentinels([0; 384]);

/// What the handlers load into the vector registers: not zero, and not any
/// sentinel.
#[repr(C, align(32))]
struct Garbage([u8; 32]);
static GARBAGE: Garbage = Garbage([0xA5; 32]);

const RCX_SENTINEL: u64 = 0x5A5A_1234_C0FF_EE01;
const R11_SENTINEL: u64 = 0x1111_FEED_FACE_0B0B;

/// Where the `SIGSEGV` handler resumes the faulting window.
static mut RESUME_AT: u64 = 0;
static mut DELIVERED: u64 = 0;
static mut FAULTS: u64 = 0;
static mut HAVE_AVX: bool = false;

#[inline(always)]
unsafe fn syscall6(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") nr => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
            in("r9") a6,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

core::arch::global_asm!(
    r#"
    .section .text._start
    .global _start
_start:
    mov rdi, rsp
    and rsp, -16
    call rust_start
1:  jmp 1b

    /* `sa_restorer`: the kernel has no trampoline page on x86_64, so every
     * handler's `ret` lands here. */
    .global restore_rt
restore_rt:
    mov rax, 15                     /* SYS_rt_sigreturn */
    syscall
2:  jmp 2b
"#
);

unsafe extern "C" {
    fn restore_rt();
}

fn exit(code: u64) -> ! {
    // SAFETY: exit_group takes nothing but the status.
    unsafe { syscall6(SYS_EXIT_GROUP, code, 0, 0, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}

fn write2(bytes: &[u8]) {
    // SAFETY: a live slice of this program's memory.
    unsafe { syscall6(SYS_WRITE, 2, bytes.as_ptr() as u64, bytes.len() as u64, 0, 0, 0) };
}

fn write_hex(label: &[u8], v: u64) {
    let mut buf = [0u8; 80];
    let mut n = 0;
    for &b in label {
        buf[n] = b;
        n += 1;
    }
    buf[n] = b'0';
    buf[n + 1] = b'x';
    n += 2;
    let mut started = false;
    for shift in (0..16).rev() {
        let nib = ((v >> (shift * 4)) & 0xF) as u8;
        if nib != 0 || started || shift == 0 {
            started = true;
            buf[n] = if nib < 10 { b'0' + nib } else { b'a' + nib - 10 };
            n += 1;
        }
    }
    buf[n] = b'\n';
    n += 1;
    write2(&buf[..n]);
}

/// Destroy everything a correct kernel must give back: `rcx`, `r11`, all
/// sixteen `xmm` (and the `ymm` upper halves where they exist).
#[inline(always)]
unsafe fn clobber() {
    unsafe {
        core::arch::asm!(
            "mov rcx, 0x0BAD0BAD0BAD0BAD",
            "mov r11, 0x0DEF0DEF0DEF0DEF",
            "movq xmm0, rcx",  "punpcklqdq xmm0, xmm0",
            "movq xmm1, rcx",  "punpcklqdq xmm1, xmm1",
            "movq xmm2, rcx",  "punpcklqdq xmm2, xmm2",
            "movq xmm3, rcx",  "punpcklqdq xmm3, xmm3",
            "movq xmm4, rcx",  "punpcklqdq xmm4, xmm4",
            "movq xmm5, rcx",  "punpcklqdq xmm5, xmm5",
            "movq xmm6, rcx",  "punpcklqdq xmm6, xmm6",
            "movq xmm7, rcx",  "punpcklqdq xmm7, xmm7",
            "movq xmm8, rcx",  "punpcklqdq xmm8, xmm8",
            "movq xmm9, rcx",  "punpcklqdq xmm9, xmm9",
            "movq xmm10, rcx", "punpcklqdq xmm10, xmm10",
            "movq xmm11, rcx", "punpcklqdq xmm11, xmm11",
            "movq xmm12, rcx", "punpcklqdq xmm12, xmm12",
            "movq xmm13, rcx", "punpcklqdq xmm13, xmm13",
            "movq xmm14, rcx", "punpcklqdq xmm14, xmm14",
            "movq xmm15, rcx", "punpcklqdq xmm15, xmm15",
            out("rcx") _, out("r11") _,
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
            out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _,
            out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _,
            out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
            options(nostack, nomem, preserves_flags)
        );
        if core::ptr::read_volatile(&raw const HAVE_AVX) {
            core::arch::asm!(
                "vmovdqu ymm0, ymmword ptr [{g}]",
                "vmovdqu ymm1, ymmword ptr [{g}]",
                "vmovdqu ymm2, ymmword ptr [{g}]",
                "vmovdqu ymm3, ymmword ptr [{g}]",
                g = in(reg) GARBAGE.0.as_ptr(),
                out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
                options(nostack, readonly, preserves_flags)
            );
        }
    }
}

extern "C" fn on_usr1(_sig: i32, _info: *mut u8, _uc: *mut u8) {
    // SAFETY: the counter is this program's own; the clobber touches registers
    // the handler is free to use.
    unsafe {
        clobber();
        let n = core::ptr::read_volatile(&raw const DELIVERED);
        core::ptr::write_volatile(&raw mut DELIVERED, n + 1);
    }
}

extern "C" fn on_segv(_sig: i32, _info: *mut u8, uc: *mut u8) {
    // SAFETY: `uc` is the kernel's `ucontext`; `uc_mcontext.rip` is at +40 +
    // 128 (`amd64/src/signal.rs` asserts both offsets). `RESUME_AT` was set by
    // the window just before the faulting store.
    unsafe {
        clobber();
        let rip = uc.add(40 + 128).cast::<u64>();
        core::ptr::write_volatile(rip, core::ptr::read_volatile(&raw const RESUME_AT));
        let n = core::ptr::read_volatile(&raw const FAULTS);
        core::ptr::write_volatile(&raw mut FAULTS, n + 1);
    }
}

/// One checked window. Returns the mismatch mask.
///
/// Two spellings of the same block — with and without the four `ymm` upper
/// halves — because the AVX instructions must not execute on a machine that
/// has no AVX, and the whole point is one block in which everything is live at
/// once.
macro_rules! window {
    ($s:expr, $fault:expr, $resume:expr, $avx_load:literal, $avx_check:literal) => {{
        let mask: u64;
        core::arch::asm!(
            // Load the sentinels.
            "movdqu xmm0,  xmmword ptr [{s} + 0]",
            "movdqu xmm1,  xmmword ptr [{s} + 16]",
            "movdqu xmm2,  xmmword ptr [{s} + 32]",
            "movdqu xmm3,  xmmword ptr [{s} + 48]",
            "movdqu xmm4,  xmmword ptr [{s} + 64]",
            "movdqu xmm5,  xmmword ptr [{s} + 80]",
            "movdqu xmm6,  xmmword ptr [{s} + 96]",
            "movdqu xmm7,  xmmword ptr [{s} + 112]",
            "movdqu xmm8,  xmmword ptr [{s} + 128]",
            "movdqu xmm9,  xmmword ptr [{s} + 144]",
            "movdqu xmm10, xmmword ptr [{s} + 160]",
            "movdqu xmm11, xmmword ptr [{s} + 176]",
            "movdqu xmm12, xmmword ptr [{s} + 192]",
            "movdqu xmm13, xmmword ptr [{s} + 208]",
            "movdqu xmm14, xmmword ptr [{s} + 224]",
            "movdqu xmm15, xmmword ptr [{s} + 240]",
            $avx_load,
            "mov rcx, {c}",
            "mov r11, {e}",
            // The optional fault, with everything live: `RESUME_AT` first.
            "test {f}, {f}",
            "jz 2f",
            "lea rax, [rip + 2f]",
            "mov qword ptr [{r}], rax",
            "mov qword ptr [0], rax",
            "2:",
            // Idle with everything live; this is where the tick lands.
            "mov {t}, {n}",
            "3:",
            "pause",
            "dec {t}",
            "jnz 3b",
            // Compare. `pcmpeqd` leaves all-ones lanes where equal, so the
            // byte mask is 0xFFFF iff the register still holds its sentinel.
            "xor eax, eax",
            "pcmpeqd xmm0,  xmmword ptr [{s} + 0]",   "pmovmskb {t:e}, xmm0",  "cmp {t:e}, 0xFFFF", "setne {t:l}", "movzx {t:e}, {t:l}", "or rax, {t}",
            "pcmpeqd xmm1,  xmmword ptr [{s} + 16]",  "pmovmskb {t:e}, xmm1",  "cmp {t:e}, 0xFFFF", "setne {t:l}", "movzx {t:e}, {t:l}", "shl {t}, 1",  "or rax, {t}",
            "pcmpeqd xmm2,  xmmword ptr [{s} + 32]",  "pmovmskb {t:e}, xmm2",  "cmp {t:e}, 0xFFFF", "setne {t:l}", "movzx {t:e}, {t:l}", "shl {t}, 2",  "or rax, {t}",
            "pcmpeqd xmm3,  xmmword ptr [{s} + 48]",  "pmovmskb {t:e}, xmm3",  "cmp {t:e}, 0xFFFF", "setne {t:l}", "movzx {t:e}, {t:l}", "shl {t}, 3",  "or rax, {t}",
            "pcmpeqd xmm4,  xmmword ptr [{s} + 64]",  "pmovmskb {t:e}, xmm4",  "cmp {t:e}, 0xFFFF", "setne {t:l}", "movzx {t:e}, {t:l}", "shl {t}, 4",  "or rax, {t}",
            "pcmpeqd xmm5,  xmmword ptr [{s} + 80]",  "pmovmskb {t:e}, xmm5",  "cmp {t:e}, 0xFFFF", "setne {t:l}", "movzx {t:e}, {t:l}", "shl {t}, 5",  "or rax, {t}",
            "pcmpeqd xmm6,  xmmword ptr [{s} + 96]",  "pmovmskb {t:e}, xmm6",  "cmp {t:e}, 0xFFFF", "setne {t:l}", "movzx {t:e}, {t:l}", "shl {t}, 6",  "or rax, {t}",
            "pcmpeqd xmm7,  xmmword ptr [{s} + 112]", "pmovmskb {t:e}, xmm7",  "cmp {t:e}, 0xFFFF", "setne {t:l}", "movzx {t:e}, {t:l}", "shl {t}, 7",  "or rax, {t}",
            "pcmpeqd xmm8,  xmmword ptr [{s} + 128]", "pmovmskb {t:e}, xmm8",  "cmp {t:e}, 0xFFFF", "setne {t:l}", "movzx {t:e}, {t:l}", "shl {t}, 8",  "or rax, {t}",
            "pcmpeqd xmm9,  xmmword ptr [{s} + 144]", "pmovmskb {t:e}, xmm9",  "cmp {t:e}, 0xFFFF", "setne {t:l}", "movzx {t:e}, {t:l}", "shl {t}, 9",  "or rax, {t}",
            "pcmpeqd xmm10, xmmword ptr [{s} + 160]", "pmovmskb {t:e}, xmm10", "cmp {t:e}, 0xFFFF", "setne {t:l}", "movzx {t:e}, {t:l}", "shl {t}, 10", "or rax, {t}",
            "pcmpeqd xmm11, xmmword ptr [{s} + 176]", "pmovmskb {t:e}, xmm11", "cmp {t:e}, 0xFFFF", "setne {t:l}", "movzx {t:e}, {t:l}", "shl {t}, 11", "or rax, {t}",
            "pcmpeqd xmm12, xmmword ptr [{s} + 192]", "pmovmskb {t:e}, xmm12", "cmp {t:e}, 0xFFFF", "setne {t:l}", "movzx {t:e}, {t:l}", "shl {t}, 12", "or rax, {t}",
            "pcmpeqd xmm13, xmmword ptr [{s} + 208]", "pmovmskb {t:e}, xmm13", "cmp {t:e}, 0xFFFF", "setne {t:l}", "movzx {t:e}, {t:l}", "shl {t}, 13", "or rax, {t}",
            "pcmpeqd xmm14, xmmword ptr [{s} + 224]", "pmovmskb {t:e}, xmm14", "cmp {t:e}, 0xFFFF", "setne {t:l}", "movzx {t:e}, {t:l}", "shl {t}, 14", "or rax, {t}",
            "pcmpeqd xmm15, xmmword ptr [{s} + 240]", "pmovmskb {t:e}, xmm15", "cmp {t:e}, 0xFFFF", "setne {t:l}", "movzx {t:e}, {t:l}", "shl {t}, 15", "or rax, {t}",
            "cmp rcx, {c}", "setne {t:l}", "movzx {t:e}, {t:l}", "shl {t}, 16", "or rax, {t}",
            "cmp r11, {e}", "setne {t:l}", "movzx {t:e}, {t:l}", "shl {t}, 17", "or rax, {t}",
            $avx_check,
            s = in(reg) $s,
            c = in(reg) RCX_SENTINEL,
            e = in(reg) R11_SENTINEL,
            f = in(reg) $fault,
            r = in(reg) $resume,
            n = in(reg) IDLE_SPINS,
            t = out(reg) _,
            out("rax") mask,
            out("rcx") _, out("r11") _,
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
            out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _,
            out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _,
            out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
            options(nostack)
        );
        mask
    }};
}

fn detect_avx() -> bool {
    // SAFETY: `cpuid` is unprivileged; `xgetbv` is legal once cpuid says
    // `OSXSAVE` (the kernel set `CR4.OSXSAVE`).
    unsafe {
        let l1 = core::arch::x86_64::__cpuid(1);
        let osxsave = l1.ecx & (1 << 27) != 0;
        let avx = l1.ecx & (1 << 28) != 0;
        if !(osxsave && avx) {
            return false;
        }
        let (lo, hi): (u32, u32);
        core::arch::asm!("xgetbv", in("ecx") 0u32, out("eax") lo, out("edx") hi, options(nostack, nomem, preserves_flags));
        let xcr0 = (u64::from(hi) << 32) | u64::from(lo);
        xcr0 & 0b110 == 0b110
    }
}

fn install(sig: u64, handler: extern "C" fn(i32, *mut u8, *mut u8)) -> bool {
    let act = KSigaction {
        handler: handler as usize as u64,
        flags: SA_SIGINFO | SA_RESTORER | SA_NODEFER,
        restorer: restore_rt as usize as u64,
        mask: 0,
    };
    // SAFETY: a well-formed sigaction on this program's own handler.
    unsafe { syscall6(SYS_RT_SIGACTION, sig, (&raw const act) as u64, 0, 8, 0, 0) == 0 }
}

fn child(parent: u64) -> ! {
    let ts = Timespec { sec: 0, nsec: CHILD_SLEEP_NS };
    for _ in 0..CHILD_SENDS {
        // SAFETY: kill and nanosleep with well-formed arguments.
        unsafe {
            if syscall6(SYS_KILL, parent, SIGUSR1, 0, 0, 0, 0) != 0 {
                break; // the parent is gone
            }
            syscall6(SYS_NANOSLEEP, (&raw const ts) as u64, 0, 0, 0, 0, 0);
        }
    }
    exit(0)
}

fn reap(child_pid: u64) {
    let mut status: i32 = 0;
    // SAFETY: wait4 on our own child with a live status word.
    unsafe { syscall6(SYS_WAIT4, child_pid, (&raw mut status) as u64, 0, 0, 0, 0) };
}

fn fail(child_pid: u64, code: u64) -> ! {
    // SAFETY: kill our own child, then reap it.
    unsafe { syscall6(SYS_KILL, child_pid, SIGKILL, 0, 0, 0, 0) };
    reap(child_pid);
    exit(code)
}

/// # Safety
/// `_sp` must be the System V initial stack pointer.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_start(_sp: *const u64) -> ! {
    // SAFETY: the statics are this program's own and nothing else runs yet.
    unsafe {
        // Distinct, non-zero lanes: byte `b` of lane `i` is `0x11 + 0x10*i + b`,
        // so two lanes never compare equal and no lane is all-zero.
        for i in 0..24usize {
            for b in 0..16usize {
                SENTINELS.0[i * 16 + b] = (0x11 + 0x10 * (i as u8)).wrapping_add(b as u8);
            }
        }
        // The `ymm` images' low halves are the `xmm` sentinels of the same number.
        for j in 0..4usize {
            for b in 0..16usize {
                SENTINELS.0[256 + 32 * j + b] = SENTINELS.0[16 * j + b];
            }
        }
        HAVE_AVX = detect_avx();
    }
    if !install(SIGUSR1, on_usr1) || !install(SIGSEGV, on_segv) {
        exit(4);
    }
    // SAFETY: getpid and fork take no pointers.
    let me = unsafe { syscall6(SYS_GETPID, 0, 0, 0, 0, 0, 0) };
    let pid = unsafe { syscall6(SYS_FORK, 0, 0, 0, 0, 0, 0) };
    if (pid as i64) < 0 {
        exit(2);
    }
    if pid == 0 {
        child(me);
    }

    let s = (&raw const SENTINELS).cast::<u8>();
    let resume = &raw mut RESUME_AT;
    // SAFETY: a plain read of a flag this thread set before forking.
    let avx = unsafe { core::ptr::read_volatile(&raw const HAVE_AVX) };
    let mut windows = 0u64;
    loop {
        let fault = u64::from(windows % FAULT_EVERY == FAULT_EVERY - 1);
        // SAFETY: the window reads the sentinels, writes `RESUME_AT`, and
        // stores through address 0 only when `fault` is set, which the SIGSEGV
        // handler resumes past.
        let mask = unsafe {
            if avx {
                window!(
                    s, fault, resume,
                    // The upper halves of `ymm0`–`ymm3`: loaded whole (low half = the
                    // `xmm` sentinel of the same number) ...
                    "vmovdqu ymm0, ymmword ptr [{s} + 256]\nvmovdqu ymm1, ymmword ptr [{s} + 288]\nvmovdqu ymm2, ymmword ptr [{s} + 320]\nvmovdqu ymm3, ymmword ptr [{s} + 352]",
                    // ... and checked by pulling the upper half down into the
                    // already-checked low register.
                    "vextractf128 xmm0, ymm0, 1\npcmpeqd xmm0, xmmword ptr [{s} + 272]\npmovmskb {t:e}, xmm0\ncmp {t:e}, 0xFFFF\nsetne {t:l}\nmovzx {t:e}, {t:l}\nshl {t}, 18\nor rax, {t}\nvextractf128 xmm1, ymm1, 1\npcmpeqd xmm1, xmmword ptr [{s} + 304]\npmovmskb {t:e}, xmm1\ncmp {t:e}, 0xFFFF\nsetne {t:l}\nmovzx {t:e}, {t:l}\nshl {t}, 19\nor rax, {t}\nvextractf128 xmm2, ymm2, 1\npcmpeqd xmm2, xmmword ptr [{s} + 336]\npmovmskb {t:e}, xmm2\ncmp {t:e}, 0xFFFF\nsetne {t:l}\nmovzx {t:e}, {t:l}\nshl {t}, 20\nor rax, {t}\nvextractf128 xmm3, ymm3, 1\npcmpeqd xmm3, xmmword ptr [{s} + 368]\npmovmskb {t:e}, xmm3\ncmp {t:e}, 0xFFFF\nsetne {t:l}\nmovzx {t:e}, {t:l}\nshl {t}, 21\nor rax, {t}\nvzeroupper"
                )
            } else {
                window!(s, fault, resume, "", "")
            }
        };
        if mask != 0 {
            write_hex(b"sigregs: mismatch mask ", mask);
            fail(pid, 1);
        }
        windows += 1;
        // SAFETY: counters the handlers bump; plain reads.
        let (delivered, faults) = unsafe {
            (core::ptr::read_volatile(&raw const DELIVERED), core::ptr::read_volatile(&raw const FAULTS))
        };
        if delivered >= WANT_DELIVERIES && faults >= WANT_FAULTS {
            break;
        }
        if windows >= MAX_WINDOWS {
            write_hex(b"sigregs: deliveries ", delivered);
            write_hex(b"sigregs: faults ", faults);
            fail(pid, if delivered < WANT_DELIVERIES { 3 } else { 5 });
        }
    }
    // SAFETY: our own child.
    unsafe { syscall6(SYS_KILL, pid, SIGKILL, 0, 0, 0, 0) };
    reap(pid);
    exit(0)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    exit(0xFF)
}
