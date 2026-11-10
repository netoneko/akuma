//! The AArch64 exception entry points.
//!
//! The vector table, the `#[no_mangle]` handlers it branches to, and the three
//! control-flow register writes (`VBAR_EL1`, `TPIDR_EL1`, `ELR_EL1`).
//!
//! Moved out of `akuma-exceptions` (2026-11-06, `docs/archive/UNSAFE_FORBID_THREE_CRATES_PLAN.md`)
//! so that crate can `forbid(unsafe_code)`. Everything here is a link-level construct
//! (`global_asm!`, `#[unsafe(no_mangle)]`, `unsafe extern`) or a claim made once at the
//! entry:
//!
//! * each shim builds the typed handle its handler wants (`FrameHandle`) from the
//!   pointer the vector asm passes, and nothing else;
//! * the four "peek" services are the crash-dump and IRQ-frame reads the logic crate
//!   cannot make itself. They are handed over as plain `fn` pointers and are only ever
//!   called on addresses the logic crate derived from live exception state.
//!
//! The logic stays in `akuma-exceptions` as safe functions: `default_exception`,
//! `irq_with_sp`, `sync_el1`, `el1_fault_recovery`, `sync_el0`.

use akuma_exceptions as exc;
use akuma_trap_aarch64::{FrameHandle, UserTrapFrame, irq_frame::IrqFrame};
use core::arch::global_asm;

// Exception vector table with EL0 support
#[cfg(target_os = "none")]
global_asm!(
    r#"
.section .text.exceptions
.balign 0x800

.global exception_vector_table
exception_vector_table:
    // Current EL with SP0 (shouldn't happen normally)
    .balign 0x80
    b sync_el1_handler              // Synchronous
    .balign 0x80
    b irq_handler                   // IRQ
    .balign 0x80
    b default_exception_handler     // FIQ
    .balign 0x80
    b default_exception_handler     // SError

    // Current EL with SPx (kernel exceptions)
    .balign 0x80
    b sync_el1_handler              // Synchronous
    .balign 0x80
    b irq_handler                   // IRQ
    .balign 0x80
    b default_exception_handler     // FIQ
    .balign 0x80
    b default_exception_handler     // SError

    // Lower EL using AArch64 (EL0 -> EL1, user mode exceptions)
    .balign 0x80
    b sync_el0_handler              // Synchronous (SVC syscalls, faults)
    .balign 0x80
    b irq_el0_handler               // IRQ
    .balign 0x80
    b default_exception_handler     // FIQ
    .balign 0x80
    b default_exception_handler     // SError

    // Lower EL using AArch32 (not supported)
    .balign 0x80
    b default_exception_handler     // Synchronous
    .balign 0x80
    b irq_handler                   // IRQ
    .balign 0x80
    b default_exception_handler     // FIQ
    .balign 0x80
    b default_exception_handler     // SError

// Default exception handler - calls Rust handler then returns
default_exception_handler:
    stp     x0, x1, [sp, #-16]!
    stp     x29, x30, [sp, #-16]!
    stp     x2, x3, [sp, #-16]!     // Save extra regs for IL bit fix
    bl      rust_default_exception_handler
    // Clear IL bit in SPSR before ERET to prevent EC=0xe
    mrs     x2, spsr_el1
    bic     x2, x2, #0x100000       // Clear IL bit (bit 20)
    msr     spsr_el1, x2
    ldp     x2, x3, [sp], #16
    ldp     x29, x30, [sp], #16
    ldp     x0, x1, [sp], #16
    eret

// Synchronous exception from EL1 (kernel fault)
//
// THIS HANDLER MUST BE TRANSPARENT TO EVERY GPR. It is not just a crash
// reporter: `try_resolve_el1_cow_fault` and `try_resolve_el1_user_copy_lazy_fault`
// both *resolve* the fault and return, and the `eret` below then re-executes the
// faulting instruction. A retry is only correct if that instruction's input
// registers still hold what they held when it aborted — so anything
// `rust_sync_el1_handler` clobbers is silently written into memory by the retry.
//
// Saving x0-x3/x29/x30 was enough only by accident: the pre-2026-08-27
// `__arch_copy_user_memory` byte loop kept its one live datum in x3. The
// 64/16/8-byte widening put live data in x3-x10, and x4-x10 were destroyed
// across the fault — so a `read(2)` whose destination page was faulted in
// mid-copy stored leftover handler state instead of file bytes, 24 bytes per
// page, while reporting the full byte count
// (docs/archive/BUSYBOX_HASH_MISCOMPUTE.md).
//
// x19-x28 need no save here: `rust_sync_el1_handler` is `extern "C"`, so AAPCS64
// obliges it to preserve them. x18 does, because Rust treats the platform
// register as a temporary on this target unless told otherwise. FP/SIMD state is
// still NOT saved — no kernel copy path holds live NEON across a store, and the
// EL0/IRQ vectors that do save it are the ones that need it.
sync_el1_handler:
    // Save the volatile registers a retried instruction may still need. FIRST,
    // so the block handed to Rust below keeps its documented layout.
    stp     x4, x5, [sp, #-16]!
    stp     x6, x7, [sp, #-16]!
    stp     x8, x9, [sp, #-16]!
    stp     x10, x11, [sp, #-16]!
    stp     x12, x13, [sp, #-16]!
    stp     x14, x15, [sp, #-16]!
    stp     x16, x17, [sp, #-16]!
    str     x18, [sp, #-16]!        // 8 bytes + 8 padding: keep sp 16-byte aligned

    stp     x29, x30, [sp, #-16]!
    stp     x0, x1, [sp, #-16]!
    stp     x2, x3, [sp, #-16]!     // Save extra regs for IL bit fix
    
    // Call Rust handler with a pointer to the saved-register block
    // (layout from the pushes above: +0 x2, +8 x3, +16 x0, +24 x1, +32 x29, +40 x30)
    mov     x0, sp
    bl      rust_sync_el1_handler
    
    // Clear IL bit in SPSR before ERET to prevent EC=0xe
    mrs     x2, spsr_el1
    bic     x2, x2, #0x100000       // Clear IL bit (bit 20)
    msr     spsr_el1, x2
    
    // Restore and return
    ldp     x2, x3, [sp], #16
    ldp     x0, x1, [sp], #16
    ldp     x29, x30, [sp], #16

    ldr     x18, [sp], #16
    ldp     x16, x17, [sp], #16
    ldp     x14, x15, [sp], #16
    ldp     x12, x13, [sp], #16
    ldp     x10, x11, [sp], #16
    ldp     x8, x9, [sp], #16
    ldp     x6, x7, [sp], #16
    ldp     x4, x5, [sp], #16
    eret

// Synchronous exception from EL0 (user mode)
// Handles SVC syscalls and user faults
//
// Frame layout (832 bytes):
//   [sp+0..287]:   UserTrapFrame (GPRs, SP_EL0, ELR, SPSR, TPIDR)
//   [sp+288..303]: kernel SP + padding
//   [sp+304..831]: NEON/FP state (Q0-Q31, FPCR, FPSR)
sync_el0_handler:
    // Allocate full frame: 304 GPR + 528 NEON = 832 bytes
    sub     sp, sp, #832
    
    // Save x8-x11 first (we'll clobber these for stack/NEON operations)
    stp     x8, x9, [sp, #64]
    stp     x10, x11, [sp, #80]
    
    // Save kernel SP at offset 288 (sp + 832 = original SP)
    add     x9, sp, #832
    str     x9, [sp, #288]
    
    // Save NEON/FP state at [sp+304..831]
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
    
    // Save x0-x7
    stp     x0, x1, [sp, #0]
    stp     x2, x3, [sp, #16]
    stp     x4, x5, [sp, #32]
    stp     x6, x7, [sp, #48]
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
    
    // Save SP_EL0
    mrs     x0, sp_el0
    str     x0, [sp, #248]
    
    // Save ELR_EL1 (user PC)
    mrs     x0, elr_el1
    str     x0, [sp, #256]
    
    // Save SPSR_EL1
    mrs     x0, spsr_el1
    str     x0, [sp, #264]

    // Save TPIDR_EL0 (TLS)
    mrs     x0, tpidr_el0
    str     x0, [sp, #272]
    
    // Pass pointer to saved context as first arg
    mov     x0, sp

    // Snapshot the trap syndrome (x1=ESR_EL1, x2=FAR_EL1) as handler args
    // while PSTATE.I is still masked from the trap. The syndrome registers
    // are per-PE and only hold THIS trap's values until the next trap on
    // this PE; IRQs are enabled just below, so any later `mrs esr_el1` /
    // `mrs far_el1` in the handler can observe a different trap's syndrome
    // (SMP phantom-SVC: an EL0 data abort classified as EC_SVC64 after the
    // BKL spin was preempted by other threads' traps).
    mrs     x1, esr_el1
    mrs     x2, far_el1

    // Enable IRQs during syscall handling to allow preemption
    msr     daifclr, #2
    isb

    // Call Rust handler - returns syscall result in x0
    bl      rust_sync_el0_handler
    
    // Disable IRQs before restoring registers
    msr     daifset, #2
    isb
    
    // x0 now has the syscall return value
    // Save it to scratch area while we restore other registers
    str     x0, [sp, #280]
    
    // Restore NEON/FP state from [sp+304..831]
    ldr     x0, [sp, #816]
    ldr     x1, [sp, #824]
    msr     fpcr, x0
    msr     fpsr, x1
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
    
    // Restore SPSR_EL1 (clear IL bit to prevent EC=0xe)
    ldr     x0, [sp, #264]
    bic     x0, x0, #0x100000
    msr     spsr_el1, x0
    
    // Restore ELR_EL1
    ldr     x0, [sp, #256]
    msr     elr_el1, x0
    
    // Restore SP_EL0
    ldr     x0, [sp, #248]
    msr     sp_el0, x0

    // Restore TPIDR_EL0 (TLS)
    ldr     x0, [sp, #272]
    msr     tpidr_el0, x0
    
    // Restore x30
    ldr     x30, [sp, #240]
    ldp     x28, x29, [sp, #224]
    ldp     x26, x27, [sp, #208]
    ldp     x24, x25, [sp, #192]
    ldp     x22, x23, [sp, #176]
    ldp     x20, x21, [sp, #160]
    ldp     x18, x19, [sp, #144]
    ldp     x16, x17, [sp, #128]
    ldp     x14, x15, [sp, #112]
    ldp     x12, x13, [sp, #96]
    ldp     x10, x11, [sp, #80]
    ldp     x8, x9, [sp, #64]
    ldp     x6, x7, [sp, #48]
    ldp     x4, x5, [sp, #32]
    ldp     x2, x3, [sp, #16]
    ldr     x1, [sp, #8]
    
    // Load syscall return value into x0
    ldr     x0, [sp, #280]
    
    // Cleanup stack frame (832 bytes)
    add     sp, sp, #832
    
    // Return to user mode
    eret

// IRQ from EL0 (user mode)
// UNIFIED: Stack-based save/restore, same mechanism as EL1 IRQ handler.
// Context switch: Rust handler returns new SP, assembly does the actual switch.
// 
// EL0 IRQ frame layout (832 bytes total) — full field list in
// `threading::setup_fake_irq_frame`, which writes this exact layout:
//   [sp+0..287]:   GPR block (x30, x28-x29, ..., x0-x1, ELR, SPSR, SP_EL0, TPIDR)
//   [sp+288..815]: NEON block (Q0-Q31 at +288, FPCR at +800, FPSR at +808)
//   [sp+816..831]: x10, x11 (scratch, pushed first / popped last)
//
// NOT interchangeable with the EL0 *sync* frame (`UserTrapFrame`), which is also
// 832 bytes but orders its GPRs differently and starts NEON at +304.
irq_el0_handler:
    // ============================================================
    // SAVE PHASE: Push all registers to stack in fixed layout
    // EL0 IRQ frame: 832 bytes (GPR + NEON/FP)
    // ============================================================
    
    // First save x10, x11 (need them for system registers)
    stp     x10, x11, [sp, #-16]!

    // Save NEON/FP state (528 bytes: 32 Q-regs + FPCR + FPSR)
    sub     sp, sp, #528
    stp     q0,  q1,  [sp, #0]
    stp     q2,  q3,  [sp, #32]
    stp     q4,  q5,  [sp, #64]
    stp     q6,  q7,  [sp, #96]
    stp     q8,  q9,  [sp, #128]
    stp     q10, q11, [sp, #160]
    stp     q12, q13, [sp, #192]
    stp     q14, q15, [sp, #224]
    stp     q16, q17, [sp, #256]
    stp     q18, q19, [sp, #288]
    stp     q20, q21, [sp, #320]
    stp     q22, q23, [sp, #352]
    stp     q24, q25, [sp, #384]
    stp     q26, q27, [sp, #416]
    stp     q28, q29, [sp, #448]
    stp     q30, q31, [sp, #480]
    mrs     x10, fpcr
    mrs     x11, fpsr
    str     x10, [sp, #512]
    str     x11, [sp, #520]

    // Save TPIDR_EL0 (TLS thread pointer)
    mrs     x10, tpidr_el0
    str     x10, [sp, #-16]!        // 8 bytes + 8 padding
    
    // Save SP_EL0 (user stack pointer) - unique to EL0 handler
    mrs     x10, sp_el0
    str     x10, [sp, #-16]!        // 8 bytes + 8 padding
    
    // Save ELR_EL1 and SPSR_EL1 to stack
    mrs     x10, elr_el1
    mrs     x11, spsr_el1
    stp     x10, x11, [sp, #-16]!
    
    // Save all other registers
    stp     x0, x1, [sp, #-16]!
    stp     x2, x3, [sp, #-16]!
    stp     x4, x5, [sp, #-16]!
    stp     x6, x7, [sp, #-16]!
    stp     x8, x9, [sp, #-16]!
    stp     x12, x13, [sp, #-16]!
    stp     x14, x15, [sp, #-16]!
    stp     x16, x17, [sp, #-16]!
    stp     x18, x19, [sp, #-16]!
    stp     x20, x21, [sp, #-16]!
    stp     x22, x23, [sp, #-16]!
    stp     x24, x25, [sp, #-16]!
    stp     x26, x27, [sp, #-16]!
    stp     x28, x29, [sp, #-16]!
    str     x30, [sp, #-16]!
    
    // Pass current SP as argument (x0)
    mov     x0, sp
    
    // Call rust handler - returns new SP in x0 (or 0 if no switch needed)
    bl      rust_irq_handler_with_sp
    
    // Check if context switch needed (x0 != 0)
    cbz     x0, 4f
    mov     sp, x0              // Switch SP in assembly!
    // This core is now OFF the outgoing thread's stack — clear its off-CPU
    // gate so peers may pick it up. Clobbers only caller-saved regs + x30,
    // all of which the restore below overwrites. Uses the incoming thread's
    // free stack space below its frame.
    bl      rust_switch_finished
4:
    
    // ============================================================
    // RESTORE PHASE: Pop all registers from (possibly new) stack
    // ============================================================
    
    // Restore general registers (reverse order of save)
    ldr     x30, [sp], #16
    ldp     x28, x29, [sp], #16
    ldp     x26, x27, [sp], #16
    ldp     x24, x25, [sp], #16
    ldp     x22, x23, [sp], #16
    ldp     x20, x21, [sp], #16
    ldp     x18, x19, [sp], #16
    ldp     x16, x17, [sp], #16
    ldp     x14, x15, [sp], #16
    ldp     x12, x13, [sp], #16
    ldp     x8, x9, [sp], #16
    ldp     x6, x7, [sp], #16
    ldp     x4, x5, [sp], #16
    ldp     x2, x3, [sp], #16
    ldp     x0, x1, [sp], #16
    
    // Restore ELR and SPSR FROM STACK
    ldp     x10, x11, [sp], #16      // x10 = ELR, x11 = SPSR
    
    // Clear IL bit in SPSR to prevent EC=0xe
    bic     x11, x11, #0x100000
    
    // Write to system registers
    msr     elr_el1, x10
    msr     spsr_el1, x11
    
    // CRITICAL: Check for ELR=0 bug before ERET
    cbnz    x10, 5f
    mov     x0, #0xDEAD
    movk    x0, #0xBEEF, lsl #16
6:  wfi
    b       6b
5:
    
    // Restore SP_EL0 (user stack pointer)
    ldr     x10, [sp], #16           // Load SP_EL0 from stack
    msr     sp_el0, x10

    // Restore TPIDR_EL0 (TLS thread pointer)
    ldr     x10, [sp], #16
    msr     tpidr_el0, x10

    // Restore NEON/FP state
    ldr     x10, [sp, #512]
    ldr     x11, [sp, #520]
    msr     fpcr, x10
    msr     fpsr, x11
    ldp     q0,  q1,  [sp, #0]
    ldp     q2,  q3,  [sp, #32]
    ldp     q4,  q5,  [sp, #64]
    ldp     q6,  q7,  [sp, #96]
    ldp     q8,  q9,  [sp, #128]
    ldp     q10, q11, [sp, #160]
    ldp     q12, q13, [sp, #192]
    ldp     q14, q15, [sp, #224]
    ldp     q16, q17, [sp, #256]
    ldp     q18, q19, [sp, #288]
    ldp     q20, q21, [sp, #320]
    ldp     q22, q23, [sp, #352]
    ldp     q24, q25, [sp, #384]
    ldp     q26, q27, [sp, #416]
    ldp     q28, q29, [sp, #448]
    ldp     q30, q31, [sp, #480]
    add     sp, sp, #528
    
    // Restore original x10, x11
    ldp     x10, x11, [sp], #16
    
    eret

// IRQ from EL1 (kernel mode)
// UNIFIED: Stack-based save/restore, same frame layout as EL0 IRQ handler.
// Context switch: Rust handler returns new SP, assembly does the actual switch.
//
// UNIFIED IRQ frame layout (832 bytes total) - same as EL0:
//   [sp+0..287]:   GPR block (x30, x28-x29, ..., x0-x1, ELR, SPSR, SP_EL0, TPIDR)
//   [sp+288..815]: NEON block (Q0-Q31 at +288, FPCR at +800, FPSR at +808)
//   [sp+816..831]: x10, x11 (scratch, outermost — inside the frame, not past it)
irq_handler:
    // ============================================================
    // SAVE PHASE: Push all registers to stack in fixed layout
    // IRQ frame: 832 bytes total (unified with EL0 handler)
    // ============================================================
    
    // First save x10, x11 (need them for system registers)
    stp     x10, x11, [sp, #-16]!

    // Save NEON/FP state (528 bytes: 32 Q-regs + FPCR + FPSR)
    sub     sp, sp, #528
    stp     q0,  q1,  [sp, #0]
    stp     q2,  q3,  [sp, #32]
    stp     q4,  q5,  [sp, #64]
    stp     q6,  q7,  [sp, #96]
    stp     q8,  q9,  [sp, #128]
    stp     q10, q11, [sp, #160]
    stp     q12, q13, [sp, #192]
    stp     q14, q15, [sp, #224]
    stp     q16, q17, [sp, #256]
    stp     q18, q19, [sp, #288]
    stp     q20, q21, [sp, #320]
    stp     q22, q23, [sp, #352]
    stp     q24, q25, [sp, #384]
    stp     q26, q27, [sp, #416]
    stp     q28, q29, [sp, #448]
    stp     q30, q31, [sp, #480]
    mrs     x10, fpcr
    mrs     x11, fpsr
    str     x10, [sp, #512]
    str     x11, [sp, #520]

    // Save TPIDR_EL0 (TLS thread pointer)
    mrs     x10, tpidr_el0
    str     x10, [sp, #-16]!        // 8 bytes + 8 padding
    
    // Save SP_EL0 - preserves user stack during syscalls and enables
    // unified frame layout between EL0 and EL1 handlers
    mrs     x10, sp_el0
    str     x10, [sp, #-16]!        // 8 bytes + 8 padding
    
    // Save ELR_EL1 and SPSR_EL1 to stack
    mrs     x10, elr_el1
    mrs     x11, spsr_el1
    stp     x10, x11, [sp, #-16]!
    
    // Save all other registers
    stp     x0, x1, [sp, #-16]!
    stp     x2, x3, [sp, #-16]!
    stp     x4, x5, [sp, #-16]!
    stp     x6, x7, [sp, #-16]!
    stp     x8, x9, [sp, #-16]!
    stp     x12, x13, [sp, #-16]!
    stp     x14, x15, [sp, #-16]!
    stp     x16, x17, [sp, #-16]!
    stp     x18, x19, [sp, #-16]!
    stp     x20, x21, [sp, #-16]!
    stp     x22, x23, [sp, #-16]!
    stp     x24, x25, [sp, #-16]!
    stp     x26, x27, [sp, #-16]!
    stp     x28, x29, [sp, #-16]!
    str     x30, [sp, #-16]!
    
    // Pass current SP as argument (x0)
    mov     x0, sp
    
    // Call rust handler - returns new SP in x0 (or 0 if no switch needed)
    bl      rust_irq_handler_with_sp
    
    // Check if context switch needed (x0 != 0)
    cbz     x0, 3f
    mov     sp, x0              // Switch SP in assembly!
    // Same off-CPU gate clear as the EL0 IRQ path — see there.
    bl      rust_switch_finished
3:
    
    // ============================================================
    // RESTORE PHASE: Pop all registers from (possibly new) stack
    // ============================================================
    
    // Restore general registers (reverse order of save)
    ldr     x30, [sp], #16
    ldp     x28, x29, [sp], #16
    ldp     x26, x27, [sp], #16
    ldp     x24, x25, [sp], #16
    ldp     x22, x23, [sp], #16
    ldp     x20, x21, [sp], #16
    ldp     x18, x19, [sp], #16
    ldp     x16, x17, [sp], #16
    ldp     x14, x15, [sp], #16
    ldp     x12, x13, [sp], #16
    ldp     x8, x9, [sp], #16
    ldp     x6, x7, [sp], #16
    ldp     x4, x5, [sp], #16
    ldp     x2, x3, [sp], #16
    ldp     x0, x1, [sp], #16
    
    // Restore ELR and SPSR FROM STACK
    ldp     x10, x11, [sp], #16      // x10 = ELR, x11 = SPSR
    
    // Clear IL bit in SPSR to prevent EC=0xe
    bic     x11, x11, #0x100000
    
    // Write to system registers
    msr     elr_el1, x10
    msr     spsr_el1, x11
    
    // CRITICAL: Check for ELR=0 bug
    cbnz    x10, 1f
    mov     x0, #0xDEAD
    movk    x0, #0xBEEF, lsl #16
2:  wfi
    b       2b
1:
    
    // Restore SP_EL0 (user stack pointer) - matches EL0 handler frame layout
    ldr     x10, [sp], #16
    msr     sp_el0, x10

    // Restore TPIDR_EL0 (TLS thread pointer)
    ldr     x10, [sp], #16
    msr     tpidr_el0, x10

    // Restore NEON/FP state
    ldr     x10, [sp, #512]
    ldr     x11, [sp, #520]
    msr     fpcr, x10
    msr     fpsr, x11
    ldp     q0,  q1,  [sp, #0]
    ldp     q2,  q3,  [sp, #32]
    ldp     q4,  q5,  [sp, #64]
    ldp     q6,  q7,  [sp, #96]
    ldp     q8,  q9,  [sp, #128]
    ldp     q10, q11, [sp, #160]
    ldp     q12, q13, [sp, #192]
    ldp     q14, q15, [sp, #224]
    ldp     q16, q17, [sp, #256]
    ldp     q18, q19, [sp, #288]
    ldp     q20, q21, [sp, #320]
    ldp     q22, q23, [sp, #352]
    ldp     q24, q25, [sp, #384]
    ldp     q26, q27, [sp, #416]
    ldp     q28, q29, [sp, #448]
    ldp     q30, q31, [sp, #480]
    add     sp, sp, #528
    
    // Restore original x10, x11
    ldp     x10, x11, [sp], #16
    
    eret
"#
);

#[cfg(target_os = "none")]
unsafe extern "C" {
    static exception_vector_table: u8;
}


// ============================================================================
// Handlers the vector table branches to
// ============================================================================

#[unsafe(no_mangle)]
extern "C" fn rust_default_exception_handler() {
    exc::default_exception();
}

#[unsafe(no_mangle)]
extern "C" fn rust_irq_handler_with_sp(current_sp: u64) -> u64 {
    exc::irq_with_sp(current_sp)
}

/// The landing pad an unrecoverable EL1 data abort in a dying process's kernel code
/// `eret`s to; see `akuma_exceptions::sync_el1`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn el1_fault_recovery_pad() {
    exc::el1_fault_recovery();
}

/// `saved_regs` points at the block the vector pushed: `[x2, x3, x0, x1, x29, x30]`
/// (see `sync_el1_handler` in the table above), or is null.
#[unsafe(no_mangle)]
extern "C" fn rust_sync_el1_handler(saved_regs: *const u64) {
    let regs = if saved_regs.is_null() {
        None
    } else {
        // SAFETY: non-null, and the vector pushed six `u64`s there before branching
        // here; the block is live on this exception's stack for the whole call.
        Some(unsafe {
            [
                saved_regs.read(),
                saved_regs.add(1).read(),
                saved_regs.add(2).read(),
                saved_regs.add(3).read(),
                saved_regs.add(4).read(),
                saved_regs.add(5).read(),
            ]
        })
    };
    match exc::sync_el1(regs) {
        exc::El1Resume::Retry => {}
        exc::El1Resume::At(addr) => set_elr_el1(addr),
        exc::El1Resume::RecoveryPad => {
            set_elr_el1(el1_fault_recovery_pad as *const () as usize as u64);
        }
    }
}

/// Syscall / EL0-fault entry. `esr`/`far` are the syndrome the vector snapshotted
/// while PSTATE.I was still masked (see `akuma_exceptions::sync_el0`).
#[unsafe(no_mangle)]
extern "C" fn rust_sync_el0_handler(frame: *mut UserTrapFrame, esr: u64, far: u64) -> u64 {
    // SAFETY: `frame` is what the vector asm just pushed for this handler on this
    // thread's kernel stack, exclusively ours until the epilogue `eret`s - the
    // handle's one contract.
    let frame = unsafe { FrameHandle::new(frame) };
    exc::sync_el0(frame, esr, far)
}

/// Redirect where the pending `eret` returns to.
fn set_elr_el1(addr: u64) {
    // SAFETY: the address is a recovery target in kernel text chosen by
    // `akuma_exceptions::sync_el1` (a registered user-copy trampoline or this
    // module's own landing pad); `ELR_EL1` is only consumed by the `eret` that ends
    // this exception.
    unsafe {
        core::arch::asm!("msr elr_el1, {}", in(reg) addr);
    }
}

// ============================================================================
// Services handed to the logic crate
// ============================================================================

/// The live IRQ trap frame at `sp`. Only ever called by `akuma_exceptions` with the SP
/// of the frame the IRQ asm is about to restore, or the one it just built.
fn irq_frame_at(sp: u64) -> IrqFrame {
    // SAFETY: see the doc comment - `sp` is a live IRQ trap frame (the scheduler hands
    // back the next thread's saved frame, or the interrupted one).
    unsafe { IrqFrame::at(sp) }
}

/// Display-only read of kernel memory in the fatal EL1 crash dump.
fn peek_u32(addr: u64) -> u32 {
    // SAFETY: the dump calls this only for an address in the kernel identity range
    // taken from ELR_EL1; display-only, and a fault in the crash path is already fatal.
    unsafe { *(addr as *const u32) }
}

/// As [`peek_u32`], for the page-table words the walk-fault dump prints.
fn peek_u64(addr: u64) -> u64 {
    // SAFETY: as [`peek_u32`]; the addresses come from TTBR0 / the L0 entry just read.
    unsafe { *(addr as *const u64) }
}

/// Copy the physical frame `pa` (a PMM / file-cache frame) into `out`.
fn read_frame_bytes(pa: usize, out: &mut [u8]) {
    // SAFETY: `pa` is a frame of RAM that no Rust object lives in: the file-page cache
    // hands out PMM frames and the verify diagnostic is its only reader here.
    unsafe { akuma_phys::Frame::from_pa(akuma_phys::PhysAddr::from_usize(pa)) }.read_bytes(0, out);
}

/// Set the current exception stack for the running thread
/// Called during context switch to update TPIDR_EL1
#[inline]
pub fn set_current_exception_stack(stack_top: u64) {
    // Bare metal only: `msr tpidr_el1` is an EL1 instruction. It *assembles* on
    // the aarch64 development host, so an ungated body would build under
    // `cargo test` and die with `SIGILL` the first time a host test reached it —
    // the trap `akuma-cpu`'s "Host builds" note records. See this crate's own
    // note of the same name.
        unsafe {
        core::arch::asm!("msr tpidr_el1, {}", in(reg) stack_top);
    }
}

/// Initialize the exception stack pointer for the boot thread
/// Must be called before any user mode code runs
pub fn init_exception_stack() {
    // Boot thread (thread 0) uses the boot stack; its early exception stack is at
    // the very top of that region. Read the STACK_TOP linker symbol (derived from
    // the linked image size in linker.ld, the same one boot.rs loads as the initial
    // SP and main.rs reads) so this can never go stale. Hardcoding the old
    // 0x40800000 once pointed the early exception stack into the kernel heap at low
    // RAM after the boot stack was relocated — see docs/LOW_MEMORY_ENVIRONMENT.md
    // "Known bug".
    unsafe extern "C" {
        static STACK_TOP: u8;
    }
    let boot_stack_top = &raw const STACK_TOP as u64;
    set_current_exception_stack(boot_stack_top);
}

/// Point this PE's `VBAR_EL1` at the kernel's exception vector table, and `isb`
/// so it is in effect before anything can take an exception.
///
/// Called once per core: by [`init`] on the BSP, and by
/// `smp_shared::secondary_shared_start` on every PSCI-woken secondary — cores
/// share one kernel image, so they share one vector table. Those were two
/// independently written copies of this `msr` until 2026-09-01 (the secondary's
/// resolved the symbol with its own `adrp`/`add` pair); a vector-table install
/// that exists twice is a vector-table install that can drift.
///
/// `msr vbar_el1` is deliberately absent from `akuma-cpu` — installing a vector
/// table is a change of control flow, so the obligation is real and stays here,
/// discharged once.
pub fn install_vbar() {
    // Bare metal only, for two independent reasons: the vector table itself is a
    // `global_asm!` blob that only assembles for the kernel target, and
    // `msr vbar_el1` is an EL1 instruction. See this crate's "Host builds" note.
    //
    // SAFETY: installs the address of this kernel's own vector table, a linker
    // symbol in `.text`. `&raw const` resolves the symbol without reading
    // through it; the `isb` makes the new base effective before the next
    // exception can be taken.
    unsafe {
        let vbar = &raw const exception_vector_table as u64;

        // Set VBAR_EL1 (Vector Base Address Register)
        core::arch::asm!(
            "msr vbar_el1, {vbar}",
            "isb",
            vbar = in(reg) vbar
        );
    }
}

/// Hand `akuma-exceptions` the crash-dump and frame reads it cannot make itself. Both
/// front ends register them: this one in [`init`], the hooked one in `crate::hooked::init`.
pub(crate) fn register_services() {
    exc::register_entry_services(exc::EntryServices {
        irq_frame_at,
        peek_u32,
        peek_u64,
        read_frame_bytes,
    });
}

/// Install exception vector table
pub fn init() {
    // Initialize exception stack before enabling exceptions
    init_exception_stack();

    register_services();

    install_vbar();

    // Enable IRQs by clearing DAIF.I, now that VBAR_EL1 is installed and
    // synchronized above — the ordering that makes taking an interrupt safe.
    akuma_primitives::irq::unmask_irqs();
}

// ---------------------------------------------------------------------------
// Regression probe: is the EL1 sync exception transparent to x4-x18?
//
// Gated on `kernel_tests` like `src/process_tests.rs`, its only caller: the
// extreme-size profile drops the boot suite, and a probe that provokes a data
// abort has no business in a build that will never assert on it.
// ---------------------------------------------------------------------------

#[cfg(kernel_tests)]
unsafe extern "C" {
    fn __el1_gpr_transparency_probe(out: *mut u64, base: u64) -> u64;
}

#[cfg(kernel_tests)]
global_asm!(
    r#"
.text
.global __el1_gpr_transparency_probe
.global __el1_gpr_transparency_landing

// x0 = out (15 u64 slots), x1 = pattern base.
// Loads `base + N` into xN for N in 4..=18, takes ONE EL1 data abort, and writes
// what those registers hold *after* the exception into `out`. The caller compares.
//
// Leaf and stackless on purpose, for the same reason __arch_copy_user_memory is:
// the recovery rewrites ELR_EL1 to the landing label below, so anything this
// function pushed would never be popped. It also keeps the live pattern out of
// x0-x3/x29/x30, which the vector has always saved — those would pass even with
// the bug.
__el1_gpr_transparency_probe:
    add     x4,  x1, #4
    add     x5,  x1, #5
    add     x6,  x1, #6
    add     x7,  x1, #7
    add     x8,  x1, #8
    add     x9,  x1, #9
    add     x10, x1, #10
    add     x11, x1, #11
    add     x12, x1, #12
    add     x13, x1, #13
    add     x14, x1, #14
    add     x15, x1, #15
    add     x16, x1, #16
    add     x17, x1, #17
    add     x18, x1, #18

    mov     x2, x0                  // out pointer: x2 is saved by the vector
    // A high user VA nothing maps or lazily backs. NOT a low one like 0x1000:
    // measured 2026-08-28, a store there did not fault during the boot suite, so
    // the first version of this test passed vacuously on a kernel with the bug.
    movz    x3, #0x6000, lsl #32    // 0x0000_6000_0000_0000
    str     x4, [x3]                // <-- the abort. ELR is rewritten to 1f, or
                                    //     a resolved fault retries this store.
__el1_gpr_transparency_landing:
1:  stp     x4,  x5,  [x2, #0]
    stp     x6,  x7,  [x2, #16]
    stp     x8,  x9,  [x2, #32]
    stp     x10, x11, [x2, #48]
    stp     x12, x13, [x2, #64]
    stp     x14, x15, [x2, #80]
    stp     x16, x17, [x2, #96]
    str     x18,      [x2, #112]
    mov     x0, #0
    ret
    "#
);

/// Which of x4..x18 an EL1 sync exception failed to preserve — a bitmask where
/// bit `n - 4` means register `xn` came back wrong. `0` is the healthy answer.
///
/// The property under test is that `sync_el1_handler` is transparent to every
/// GPR, because `try_resolve_el1_cow_fault` and
/// `try_resolve_el1_user_copy_lazy_fault` resolve the fault and `eret` back to
/// **re-execute the faulting instruction**. A retry is only correct if that
/// instruction's inputs survived, so a register the handler clobbers is stored
/// into memory by the retry as if it were data.
///
/// The vector used to save x0-x3/x29/x30 only. That was enough while
/// `__arch_copy_user_memory` was a byte loop holding its one live datum in x3;
/// the 2026-08-27 widening put live data in x3-x10, and a `read(2)` whose
/// destination page faulted in mid-copy then stored leftover handler state —
/// 24 bytes per page, with `read` reporting the full count
/// (`docs/archive/BUSYBOX_HASH_MISCOMPUTE.md`).
///
/// Runs on whichever recovery arm the fault takes: the resolve-and-retry paths if
/// the VA turns out to be lazily mappable, otherwise the registered user-copy
/// trampoline. Both return through this same vector, so either pins the fix.
///
/// Returns `(clobber_mask, aborts_taken)`. **`aborts_taken` is not decoration** —
/// if VA `0x1000` were mapped in the current address space the store would simply
/// succeed, no exception would fire, and the mask would read `0` for a kernel with
/// the bug wide open. A test that cannot fail is worse than no test
/// (`docs/archive/SMP_ADOPTED_IDLE_STACK_CLOBBER.md` is the local precedent), so
/// the caller must require that the count moved.
#[must_use]
#[cfg(kernel_tests)]
pub fn el1_sync_gpr_clobber_mask() -> (u32, u64) {
    const BASE: u64 = 0x5A5A_0000;
    const EC_DATA_ABORT_EL1: usize = 0x25;
    let mut out = [0u64; 15];

    let before = exc::SYNC_EC_EL1[EC_DATA_ABORT_EL1].load(core::sync::atomic::Ordering::Relaxed);

    // The trampoline arm needs a registered handler, or an unrecoverable EL1
    // abort would kill the current process instead of landing at the label.
    akuma_exec::threading::set_user_copy_fault_handler(
        __el1_gpr_transparency_landing as *const () as usize as u64,
    );
    // SAFETY: `out` has the 15 slots the probe writes, and the abort it takes is
    // recovered by the handler registered just above.
    unsafe { __el1_gpr_transparency_probe(out.as_mut_ptr(), BASE) };
    akuma_exec::threading::set_user_copy_fault_handler(0);

    let after = exc::SYNC_EC_EL1[EC_DATA_ABORT_EL1].load(core::sync::atomic::Ordering::Relaxed);

    let mut mask = 0u32;
    for (i, got) in out.iter().enumerate() {
        if *got != BASE + (i as u64 + 4) {
            mask |= 1 << i;
        }
    }
    (mask, after.wrapping_sub(before))
}

#[cfg(kernel_tests)]
unsafe extern "C" {
    fn __el1_gpr_transparency_landing();
}
