//! The EL1 vector table of a hooked-scheduler AArch64 kernel.
//!
//! The full AArch64 kernel's table (`akuma-exceptions`) switches threads by
//! handing the asm a new stack pointer from its IRQ handler. A kernel that
//! schedules through `akuma-threading`'s hooked path (the tokio kernel)
//! switches inside an ordinary Rust call instead, the way amd64 does. Its
//! vectors only have to do three things: save the interrupted register file,
//! call the handler registered for that kind of entry with a `&mut Frame`, and
//! restore and `eret`.
//!
//! # The one contract
//!
//! The frame a handler receives was pushed by this crate's stub on the current
//! thread's kernel stack and stays there, unaliased, until the handler returns
//! to the stub. That is discharged once, in [`dispatch`]; handlers are safe
//! `fn(&mut Frame)`, registered once per [`Kind`] with [`register`].
//!
//! # Where an EL0 trap lands
//!
//! On `SP_EL1` as `eret` left it. [`enter_user`] sets it to the top of the
//! thread's kernel stack before its `eret`, and the stubs pop exactly what they
//! pushed, so every trap from EL0 builds its frame at the same address: the
//! top of that thread's kernel stack, minus [`Frame`]'s size. (A thread that
//! blocks in a syscall is switched away *below* its frame; the switch keeps
//! its own stack pointer.) This is what the amd64 kernel's per-slot TSS `rsp0`
//! does, with nothing to load on a switch.
//!
//! # What is saved
//!
//! Everything userspace can observe: x0-x30, `SP_EL0`, `ELR_EL1`, `SPSR_EL1`,
//! `TPIDR_EL0`, q0-q31, `FPCR`, `FPSR`. The kernel is built with FP/SIMD, so
//! a handler may clobber any of them; the syndrome (`ESR_EL1`, `FAR_EL1`) is
//! snapshotted before the handler runs, so a nested trap cannot change what it
//! reports. Interrupts stay masked from entry to `eret`: the kernel runs
//! IRQ-masked except for the windows it opens itself.
//!
//! An EL1 entry saves the same frame (with the interrupted `SP_EL1` in `sp`)
//! and restores the general and FP registers, `ELR_EL1` and `SPSR_EL1`; it
//! leaves `SP_EL0` and `TPIDR_EL0` alone, since they belong to whatever user
//! thread the kernel was serving.

#![no_std]

use akuma_primitives::OnceCopy;

/// The saved register file, as the stubs lay it out.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct Frame {
    /// x0..x30.
    pub x: [u64; 31],
    /// `SP_EL0` for an EL0 entry; the interrupted `SP_EL1` for an EL1 entry (not restored).
    pub sp: u64,
    /// `ELR_EL1`: where `eret` resumes.
    pub elr: u64,
    /// `SPSR_EL1`: the interrupted `PSTATE`.
    pub spsr: u64,
    /// `TPIDR_EL0`: userspace's TLS base (EL0 entries only).
    pub tpidr: u64,
    /// `ESR_EL1` at entry (not restored).
    pub esr: u64,
    /// `FAR_EL1` at entry (not restored).
    pub far: u64,
    _pad: u64,
    /// q0..q31.
    pub q: [u128; 32],
    pub fpcr: u64,
    pub fpsr: u64,
}

const _: () = {
    assert!(core::mem::size_of::<Frame>() == FRAME_BYTES);
    assert!(core::mem::offset_of!(Frame, sp) == 248);
    assert!(core::mem::offset_of!(Frame, elr) == 256);
    assert!(core::mem::offset_of!(Frame, spsr) == 264);
    assert!(core::mem::offset_of!(Frame, tpidr) == 272);
    assert!(core::mem::offset_of!(Frame, esr) == 280);
    assert!(core::mem::offset_of!(Frame, far) == 288);
    assert!(core::mem::offset_of!(Frame, q) == 304);
    assert!(core::mem::offset_of!(Frame, fpcr) == 816);
    assert!(core::mem::offset_of!(Frame, fpsr) == 824);
};

/// Bytes of one [`Frame`] on the stack.
pub const FRAME_BYTES: usize = 832;

/// `SPSR_EL1` for a fresh EL0 thread: `M = EL0t`, all of `DAIF` clear.
pub const SPSR_EL0: u64 = 0;

impl Frame {
    /// An all-zero frame: what a fresh thread's registers hold before its
    /// entry, stack and arguments are filled in.
    #[must_use]
    pub const fn zeroed() -> Self {
        Self {
            x: [0; 31],
            sp: 0,
            elr: 0,
            spsr: SPSR_EL0,
            tpidr: 0,
            esr: 0,
            far: 0,
            _pad: 0,
            q: [0; 32],
            fpcr: 0,
            fpsr: 0,
        }
    }

    /// The exception class (`ESR_EL1.EC`).
    #[must_use]
    pub const fn ec(&self) -> u64 {
        (self.esr >> 26) & 0x3F
    }

    /// Did this entry come from EL0?
    #[must_use]
    pub const fn from_el0(&self) -> bool {
        self.spsr.trailing_zeros() >= 4
    }
}

/// Exception classes a handler distinguishes.
pub mod ec {
    /// `svc` from AArch64.
    pub const SVC64: u64 = 0x15;
    /// Instruction abort from a lower EL.
    pub const IABT_LOWER: u64 = 0x20;
    /// Instruction abort at the current EL.
    pub const IABT_SAME: u64 = 0x21;
    /// Data abort from a lower EL.
    pub const DABT_LOWER: u64 = 0x24;
    /// Data abort at the current EL.
    pub const DABT_SAME: u64 = 0x25;
    /// A trapped `MSR`/`MRS`/system instruction.
    pub const SYS64: u64 = 0x18;
    /// `brk`.
    pub const BRK64: u64 = 0x3C;
}

/// Which vector an entry came through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u64)]
pub enum Kind {
    /// Synchronous, from EL1 (a kernel fault, or a kernel access to a lazy user page).
    El1Sync = 0,
    /// IRQ while the kernel ran.
    El1Irq = 1,
    /// Synchronous, from EL0 (`svc`, a user fault).
    El0Sync = 2,
    /// IRQ while userspace ran.
    El0Irq = 3,
    /// FIQ, SError, AArch32, or the SP_EL0 vectors: never expected.
    Unexpected = 4,
}

impl Kind {
    const COUNT: usize = 5;
    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    const fn from_raw(v: u64) -> Self {
        match v {
            0 => Self::El1Sync,
            1 => Self::El1Irq,
            2 => Self::El0Sync,
            3 => Self::El0Irq,
            _ => Self::Unexpected,
        }
    }
}

/// A handler: runs with interrupts masked, may edit the frame (the `eret`
/// resumes from what it leaves), and may switch threads (the frame stays put
/// on this thread's stack).
pub type Handler = fn(&mut Frame);

static HANDLERS: [OnceCopy<Handler>; Kind::COUNT] = [const { OnceCopy::new() }; Kind::COUNT];

/// Register the handler for `kind`. Once per kind; a second call is ignored.
pub fn register(kind: Kind, handler: Handler) {
    HANDLERS[kind as usize].set(handler);
}

/// The stubs' landing: name the frame once, then call the registered handler.
/// An entry with no handler parks the core: a boot-order bug, with nothing
/// sound to return to.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[unsafe(no_mangle)]
extern "C" fn akuma_vectors_dispatch(frame: *mut Frame, kind: u64) {
    // SAFETY: the module contract: `frame` is the frame the stub just pushed on
    // this thread's kernel stack, live and exclusively ours until we return.
    let frame = unsafe { &mut *frame };
    dispatch(frame, Kind::from_raw(kind));
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn dispatch(frame: &mut Frame, kind: Kind) {
    if let Some(h) = HANDLERS[kind as usize].get() {
        h(frame);
        return;
    }
    if let Some(h) = HANDLERS[Kind::Unexpected as usize].get() {
        h(frame);
    }
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
core::arch::global_asm!(
    r#"
.macro AKV_SAVE_COMMON
    sub     sp, sp, #832
    stp     x0, x1, [sp, #0]
    stp     x2, x3, [sp, #16]
    stp     x4, x5, [sp, #32]
    stp     x6, x7, [sp, #48]
    stp     x8, x9, [sp, #64]
    stp     x10, x11, [sp, #80]
    stp     x12, x13, [sp, #96]
    stp     x14, x15, [sp, #112]
    stp     x16, x17, [sp, #128]
    stp     x18, x19, [sp, #144]
    stp     x20, x21, [sp, #160]
    stp     x22, x23, [sp, #176]
    stp     x24, x25, [sp, #192]
    stp     x26, x27, [sp, #208]
    stp     x28, x29, [sp, #224]
    str     x30, [sp, #240]
    stp     q0,  q1,  [sp, #304]
    stp     q2,  q3,  [sp, #336]
    stp     q4,  q5,  [sp, #368]
    stp     q6,  q7,  [sp, #400]
    stp     q8,  q9,  [sp, #432]
    stp     q10, q11, [sp, #464]
    stp     q12, q13, [sp, #496]
    stp     q14, q15, [sp, #528]
    stp     q16, q17, [sp, #560]
    stp     q18, q19, [sp, #592]
    stp     q20, q21, [sp, #624]
    stp     q22, q23, [sp, #656]
    stp     q24, q25, [sp, #688]
    stp     q26, q27, [sp, #720]
    stp     q28, q29, [sp, #752]
    stp     q30, q31, [sp, #784]
    mrs     x10, fpcr
    mrs     x11, fpsr
    str     x10, [sp, #816]
    str     x11, [sp, #824]
    mrs     x10, elr_el1
    mrs     x11, spsr_el1
    stp     x10, x11, [sp, #256]
    mrs     x10, esr_el1
    mrs     x11, far_el1
    stp     x10, x11, [sp, #280]
.endm

.macro AKV_ENTRY_EL0 kind
    AKV_SAVE_COMMON
    mrs     x10, sp_el0
    str     x10, [sp, #248]
    mrs     x10, tpidr_el0
    str     x10, [sp, #272]
    mov     x0, sp
    mov     x1, #\kind
    bl      akuma_vectors_dispatch
    b       akuma_vectors_return_el0
.endm

.macro AKV_ENTRY_EL1 kind
    AKV_SAVE_COMMON
    add     x10, sp, #832
    str     x10, [sp, #248]
    mov     x0, sp
    mov     x1, #\kind
    bl      akuma_vectors_dispatch
    b       akuma_vectors_return_el1
.endm

.section .text.akuma_vectors, "ax"
.balign 0x800
.global akuma_vectors_table
akuma_vectors_table:
    // Current EL, SP_EL0: never used (the kernel runs on SP_EL1).
    .balign 0x80
    b       akv_unexpected
    .balign 0x80
    b       akv_unexpected
    .balign 0x80
    b       akv_unexpected
    .balign 0x80
    b       akv_unexpected
    // Current EL, SP_ELx: the kernel.
    .balign 0x80
    b       akv_el1_sync
    .balign 0x80
    b       akv_el1_irq
    .balign 0x80
    b       akv_unexpected
    .balign 0x80
    b       akv_unexpected
    // Lower EL, AArch64: userspace.
    .balign 0x80
    b       akv_el0_sync
    .balign 0x80
    b       akv_el0_irq
    .balign 0x80
    b       akv_unexpected
    .balign 0x80
    b       akv_unexpected
    // Lower EL, AArch32: never.
    .balign 0x80
    b       akv_unexpected
    .balign 0x80
    b       akv_unexpected
    .balign 0x80
    b       akv_unexpected
    .balign 0x80
    b       akv_unexpected

akv_el1_sync:
    AKV_ENTRY_EL1 0
akv_el1_irq:
    AKV_ENTRY_EL1 1
akv_el0_sync:
    AKV_ENTRY_EL0 2
akv_el0_irq:
    AKV_ENTRY_EL0 3
akv_unexpected:
    AKV_ENTRY_EL1 4

// Restore the FP and general registers and ELR/SPSR from the frame at sp,
// pop it, eret. EL0 additionally restores SP_EL0 and TPIDR_EL0.
.global akuma_vectors_return_el0
akuma_vectors_return_el0:
    ldr     x10, [sp, #248]
    msr     sp_el0, x10
    ldr     x10, [sp, #272]
    msr     tpidr_el0, x10
akuma_vectors_return_el1:
    ldr     x10, [sp, #816]
    ldr     x11, [sp, #824]
    msr     fpcr, x10
    msr     fpsr, x11
    ldp     q0,  q1,  [sp, #304]
    ldp     q2,  q3,  [sp, #336]
    ldp     q4,  q5,  [sp, #368]
    ldp     q6,  q7,  [sp, #400]
    ldp     q8,  q9,  [sp, #432]
    ldp     q10, q11, [sp, #464]
    ldp     q12, q13, [sp, #496]
    ldp     q14, q15, [sp, #528]
    ldp     q16, q17, [sp, #560]
    ldp     q18, q19, [sp, #592]
    ldp     q20, q21, [sp, #624]
    ldp     q22, q23, [sp, #656]
    ldp     q24, q25, [sp, #688]
    ldp     q26, q27, [sp, #720]
    ldp     q28, q29, [sp, #752]
    ldp     q30, q31, [sp, #784]
    ldp     x10, x11, [sp, #256]
    // IL is RES0 for an AArch64 eret target; a stale one makes the return an
    // illegal-state exception.
    bic     x11, x11, #0x100000
    msr     elr_el1, x10
    msr     spsr_el1, x11
    ldp     x0, x1, [sp, #0]
    ldp     x2, x3, [sp, #16]
    ldp     x4, x5, [sp, #32]
    ldp     x6, x7, [sp, #48]
    ldp     x8, x9, [sp, #64]
    ldp     x10, x11, [sp, #80]
    ldp     x12, x13, [sp, #96]
    ldp     x14, x15, [sp, #112]
    ldp     x16, x17, [sp, #128]
    ldp     x18, x19, [sp, #144]
    ldp     x20, x21, [sp, #160]
    ldp     x22, x23, [sp, #176]
    ldp     x24, x25, [sp, #192]
    ldp     x26, x27, [sp, #208]
    ldp     x28, x29, [sp, #224]
    ldr     x30, [sp, #240]
    add     sp, sp, #832
    eret

// enter_user(frame: *const Frame, kstack_top: u64) -> !
// Everything is read from x0's frame; sp is moved to the stack top before the
// last loads and nothing is written after that, so the frame may live anywhere,
// including inside the stack being reset.
.global akuma_vectors_enter_user
akuma_vectors_enter_user:
    msr     daifset, #0xf
    ldr     x10, [x0, #816]
    ldr     x11, [x0, #824]
    msr     fpcr, x10
    msr     fpsr, x11
    ldp     q0,  q1,  [x0, #304]
    ldp     q2,  q3,  [x0, #336]
    ldp     q4,  q5,  [x0, #368]
    ldp     q6,  q7,  [x0, #400]
    ldp     q8,  q9,  [x0, #432]
    ldp     q10, q11, [x0, #464]
    ldp     q12, q13, [x0, #496]
    ldp     q14, q15, [x0, #528]
    ldp     q16, q17, [x0, #560]
    ldp     q18, q19, [x0, #592]
    ldp     q20, q21, [x0, #624]
    ldp     q22, q23, [x0, #656]
    ldp     q24, q25, [x0, #688]
    ldp     q26, q27, [x0, #720]
    ldp     q28, q29, [x0, #752]
    ldp     q30, q31, [x0, #784]
    ldr     x10, [x0, #248]
    msr     sp_el0, x10
    ldr     x10, [x0, #272]
    msr     tpidr_el0, x10
    ldp     x10, x11, [x0, #256]
    bic     x11, x11, #0x100000
    msr     elr_el1, x10
    msr     spsr_el1, x11
    mov     sp, x1
    ldp     x2, x3, [x0, #16]
    ldp     x4, x5, [x0, #32]
    ldp     x6, x7, [x0, #48]
    ldp     x8, x9, [x0, #64]
    ldp     x10, x11, [x0, #80]
    ldp     x12, x13, [x0, #96]
    ldp     x14, x15, [x0, #112]
    ldp     x16, x17, [x0, #128]
    ldp     x18, x19, [x0, #144]
    ldp     x20, x21, [x0, #160]
    ldp     x22, x23, [x0, #176]
    ldp     x24, x25, [x0, #192]
    ldp     x26, x27, [x0, #208]
    ldp     x28, x29, [x0, #224]
    ldr     x30, [x0, #240]
    ldp     x0, x1, [x0, #0]
    eret
"#
);

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
unsafe extern "C" {
    static akuma_vectors_table: u8;
    fn akuma_vectors_enter_user(frame: *const Frame, kstack_top: u64) -> !;
}

/// Point `VBAR_EL1` at this crate's table. Once per core, before anything can trap.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn install() {
    let vbar = &raw const akuma_vectors_table as u64;
    // SAFETY: `vbar` is this crate's 2 KiB-aligned table, every entry of which
    // saves, dispatches and restores; writing VBAR_EL1 redirects only this core.
    unsafe {
        core::arch::asm!("msr vbar_el1, {v}", "isb", v = in(reg) vbar, options(nostack, preserves_flags));
    }
}

/// Enter EL0 at `frame` (its `elr`, `sp`, `spsr`, every register), with this
/// thread's EL0 traps landing at `kstack_top` from now on. Interrupts are
/// masked first; `frame.spsr` decides whether they are on in EL0.
///
/// The `SPSR_EL1` mode field is forced to EL0t: this crossing never returns to
/// EL1, whatever the frame says.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn enter_user(frame: &Frame, kstack_top: u64) -> ! {
    let mut f = *frame;
    f.spsr &= !0x1F;
    // SAFETY: `f` is a whole frame whose `spsr` targets EL0t, so the `eret`
    // can only reach unprivileged code; `kstack_top` is the caller's thread's
    // kernel stack top (16-byte aligned by the caller), and nothing of this
    // call's frame is used after the stack pointer moves there.
    unsafe { akuma_vectors_enter_user(&raw const f, kstack_top & !0xF) }
}
