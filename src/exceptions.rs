// ARM64 Exception handling
//!
//! Exception vectors for AArch64 with proper EL0 (user mode) support.
//!
//! When a user process makes a syscall (SVC instruction):
//! 1. CPU automatically switches to EL1 and SP_EL1
//! 2. sync_el0_handler saves all user registers
//! 3. Rust syscall handler processes the request
//! 4. Handler returns, registers restored, ERET to EL0

use core::arch::global_asm;

// Exception vector table with EL0 support
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
sync_el1_handler:
    // Save minimal context
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

unsafe extern "C" {
    static exception_vector_table: u8;
}

// ============================================================================
// Per-Thread Exception Stacks
// ============================================================================
//
// Each kernel thread has its own exception stack area reserved at the top of
// its kernel stack. This allows safe context switching during syscalls because
// each thread's trap frame is isolated.
//
// Stack layout (per thread):
// |------------------| <- stack_top (highest address)
// | Exception area   |  1KB reserved for UserTrapFrame + scratch
// |------------------|
// | Kernel stack     |  Rest of stack for normal kernel code
// |------------------| <- stack_base (lowest address)
//
// The exception stack pointer is stored in TPIDR_EL1 (Thread Pointer ID Register).
// This is a CPU register specifically designed for per-thread data access.
// On every context switch, TPIDR_EL1 is set to the new thread's exception stack.
// The sync_el0_handler reads TPIDR_EL1 directly - no global variable needed.
//
// To move exception stacks elsewhere (e.g., separate allocation):
// 1. Allocate separate memory per thread
// 2. Store pointer in ThreadSlot.exception_stack_top  
// 3. No other changes needed - scheduler reads from ThreadSlot
//
// See docs/WAIT_QUEUES.md for detailed documentation.
// ============================================================================

/// Set the current exception stack for the running thread
/// Called during context switch to update TPIDR_EL1
#[inline]
pub fn set_current_exception_stack(stack_top: u64) {
    unsafe {
        core::arch::asm!("msr tpidr_el1, {}", in(reg) stack_top);
    }
}

/// Get the current exception stack pointer from TPIDR_EL1
#[inline]
#[allow(dead_code)]
pub fn get_current_exception_stack() -> u64 {
    let val: u64;
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el1", out(reg) val);
    }
    val
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

pub use akuma_exec::threading::UserTrapFrame;

/// ESR_EL1 exception class values
mod esr {
    pub const EC_SVC64: u64 = 0b010101; // SVC instruction from AArch64
    pub const EC_DATA_ABORT_LOWER: u64 = 0b100100; // Data abort from lower EL
    pub const EC_INST_ABORT_LOWER: u64 = 0b100000; // Instruction abort from lower EL
    pub const EC_MSR_MRS_TRAP: u64 = 0b011000; // Trapped MSR/MRS/System instruction from EL0
    pub const EC_BRK_AARCH64: u64 = 0b111100; // BRK instruction from AArch64
}

// Signal frame layout constants (Linux AArch64 compatible)
const SA_SIGINFO: u64 = 4;
const SA_ONSTACK: u64 = 0x08000000;
const SA_NODEFER: u64 = 0x40000000;

/// The `rt_sigframe` itself — `#[repr(C)]` types plus the offsets derived from them,
/// which is where the layout comment that used to sit here now lives (including the
/// two deliberate divergences from Linux's `sigcontext`). The literals below were the
/// only definition until Phase 7; they are now re-exports, and any drift between the
/// struct and them is a compile error inside that module.
use akuma_exec::threading::sigframe;

// `SIGFRAME_MCONTEXT`/`SIGFRAME_FPSIMD` are deliberately not imported here: the
// struct owns those offsets now, and the only remaining users are the
// `kernel_tests` re-exports below, which spell them out. Importing them made the
// three `no-tests` profiles fail on an unused import.
use sigframe::{FPSIMD_MAGIC, SIGFRAME_SIGINFO, SIGFRAME_SIZE, SIGFRAME_UCONTEXT};

// Exposed for kernel layout tests.
#[cfg(kernel_tests)]
pub const TEST_SIGFRAME_SIZE: usize = SIGFRAME_SIZE;
#[cfg(kernel_tests)]
pub const TEST_SIGFRAME_UCONTEXT: usize = SIGFRAME_UCONTEXT;
#[cfg(kernel_tests)]
pub const TEST_SIGFRAME_MCONTEXT: usize = sigframe::SIGFRAME_MCONTEXT;
#[cfg(kernel_tests)]
pub const TEST_SIGFRAME_FPSIMD: usize = sigframe::SIGFRAME_FPSIMD;
/// Byte offset of uc_sigmask within the signal frame (ucontext_t + 40).
#[cfg(kernel_tests)]
pub const TEST_SIGFRAME_UC_SIGMASK: usize = sigframe::SIGFRAME_UC_SIGMASK;

/// Log when a per-page demand-paging slot had to be reclaimed from a previous
/// holder. A `Dead` cause is the smoking gun for the build-script deadlock
/// (a thread died mid-fault, e.g. an orphaned rustc probe child, leaving the
/// slot poisoned); `Wedged` means the bounded fallback fired. The
/// common `Acquired`/`NoProc` paths print nothing (hot path) — which arm is
/// silent is decided by `FaultSlot::reclaim_report`, host-tested in `akuma-exec`.
#[inline]
fn log_fault_reclaim(pid: u32, page_va: usize, slot: akuma_exec::process::FaultSlot) {
    use akuma_exec::process::ReclaimCause;
    match slot.reclaim_report() {
        Some((ReclaimCause::Dead, holder)) => crate::safe_print!(192,
            "[FAULT-RECLAIM] pid={} tid={} page={:#x}: holder tid={} DIED mid-fault — slot reclaimed\n",
            pid, akuma_exec::threading::current_thread_id(), page_va, holder),
        Some((ReclaimCause::Wedged, holder)) => crate::safe_print!(192,
            "[FAULT-RECLAIM] pid={} page={:#x}: holder tid={} WEDGED past spin bound — slot reclaimed\n",
            pid, page_va, holder),
        None => {}
    }
}

/// RAII holder for the per-page demand-paging serialization slot.
///
/// Constructed only by [`fault_slot_hold`]: the acquire and the release travel
/// together, so there is deliberately no way to build one of these without
/// acquiring the slot first (`fault_slot_release` is `akuma-exec`'s and stays
/// there — this side publishes no bare "release the slot" helper).
///
/// The `Drop` releases on **all** exit paths from a fault block, including the
/// early `return unsafe { (*frame).x0 }` successes and fall-through to SIGSEGV.
/// Release is holder-gated inside `fault_slot_release` — a sibling that reclaimed
/// the slot from us keeps it — which is also why running it after a `NoProc`
/// acquire is a no-op rather than a breach of the pairing contract.
///
/// The one acquire outcome that must **not** release is `AlreadyHeld` (a
/// re-entrant acquire by this same thread): the entry belongs to the outer guard,
/// and holder-gating cannot tell the two apart because the holder tid is the same.
/// That is what `owns_release` is for — see `COW_PILE_AUDIT.md` §9 F6.
struct FaultSlotGuard {
    /// The **address-space owner** pid the slot was acquired under, never the
    /// faulting thread's own pid: `CLONE_VM` siblings serialize on the
    /// thread-group leader's one `fault_mutex`. (All three predecessors of this
    /// guard named this field `pid` and assigned `as_owner` to it.)
    as_owner: u32,
    page_va: usize,
    /// False only for a nested acquire, where an outer guard owns the release.
    owns_release: bool,
}

impl Drop for FaultSlotGuard {
    fn drop(&mut self) {
        if self.owns_release {
            akuma_exec::process::fault_slot_release(self.as_owner, self.page_va);
        }
    }
}

/// Acquire the per-page fault slot for `page_va` under `as_owner`, trace it if it
/// had to be reclaimed, and hand back the guard that releases it.
///
/// `pid` is the faulting process id and is used **only** for the reclaim trace;
/// the serialization itself is always keyed on `as_owner`.
///
/// This replaced three byte-identical guards (`CowFaultGuard`, `DaFaultGuard`,
/// `FaultGuard`) in the EL0 CoW, data-abort and instruction-abort arms; the
/// call-site comments say what each one is serializing against.
///
/// A nested acquire yields a guard that does not release and prints a tripwire.
/// It should be unreachable: all three call sites are mutually exclusive branches
/// of `rust_sync_el0_handler_inner`, and that function cannot re-enter itself
/// (a fault taken while it runs comes from EL1 and takes the EL1 vector, which
/// holds no fault slot). `[FAULT-SLOT NESTED]` printing means a fourth call site
/// was added inside a fault block — the change is safe, but the new site needs
/// looking at, because it is serializing against a slot it does not own.
#[must_use = "the fault slot is released when the guard drops — discarding it \
              immediately un-serializes the page"]
#[inline]
fn fault_slot_hold(pid: u32, as_owner: u32, page_va: usize) -> FaultSlotGuard {
    let slot = akuma_exec::process::fault_slot_acquire(as_owner, page_va);
    let owns_release = !matches!(slot, akuma_exec::process::FaultSlot::AlreadyHeld);
    if !owns_release {
        crate::safe_print!(160,
            "[FAULT-SLOT NESTED] pid={} tid={} page={:#x}: re-entrant acquire — outer guard keeps the release\n",
            pid, akuma_exec::threading::current_thread_id(), page_va);
    }
    log_fault_reclaim(pid, page_va, slot);
    FaultSlotGuard { as_owner, page_va, owns_release }
}

/// True if `far` is in the kernel identity-RAM VA window (normally UXN for EL0 execute).
///
/// Used when deciding whether an EL0 instruction abort might be “stale translation” vs
/// a deliberate fault from jumping into kernel RAM.
#[inline]
pub fn far_in_kernel_identity_user_range(far: u64) -> bool {
    let a = far as usize;
    a >= akuma_exec::process::types::ProcessMemory::KERNEL_VA_START
        && a < akuma_exec::mmu::kernel_va_end()
}

/// Emulate `DC ZVA` for EL0 when QEMU TCG still traps it despite SCTLR_EL1.DZE=1.
/// Zeros the naturally-aligned block that contains `addr`, using the block size
/// from DCZID_EL0.BS (4 << BS bytes, typically 64).
pub fn emulate_dc_zva(addr: u64) {
    let dczid: u64;
    unsafe { core::arch::asm!("mrs {}, dczid_el0", out(reg) dczid); }
    // Bit 4 (DZP) set means DC ZVA is prohibited; skip silently.
    if dczid & (1 << 4) != 0 { return; }
    let bs = (dczid & 0xF) as u32;
    // block_size = 4 << BS; cap at 2048 to bound the stack frame.
    let block_size = (4usize << bs).min(2048);
    let aligned_addr = addr & !(block_size as u64 - 1);
    let zeros = [0u8; 2048];
    // Fault-time store: `Prefault::No` for the reason in `read_user_instr`.
    let _ = akuma_exec::mmu::user_access::copy_to_user_with(
        aligned_addr,
        &zeros[..block_size],
        akuma_exec::mmu::user_access::Prefault::No,
    );
}

/// Read one 32-bit instruction word from user memory **at fault time**.
///
/// `Prefault::No` is mandatory here, not a preference: every caller runs inside an
/// exception handler, where [`prefault_user_range`] would allocate frames, take the
/// address space's `as_lock` and possibly read a file — re-entering the very path
/// that is currently faulting. A lazy page hit by a kernel→user copy is instead
/// resolved in place by [`try_resolve_el1_user_copy_lazy_fault`], which is the
/// mechanism designed for it.
///
/// [`prefault_user_range`]: akuma_exec::mmu::user_access::prefault_user_range
fn read_user_instr(va: u64) -> Option<u32> {
    let mut buf = [0u8; 4];
    akuma_exec::mmu::user_access::copy_from_user_with(
        &mut buf,
        va,
        akuma_exec::mmu::user_access::Prefault::No,
    )
    .ok()
    .map(|()| u32::from_le_bytes(buf))
}

/// Emulate `stp xzr, xzr, [Xn, #imm7*8]` for EL0 when QEMU TCG misroutes it as EC=0x15.
/// Writes 16 zero bytes to `addr`.
fn emulate_stp_xzr_xzr(addr: u64) {
    let zeros = [0u8; 16];
    let _ = akuma_exec::mmu::user_access::copy_to_user_with(
        addr,
        &zeros,
        akuma_exec::mmu::user_access::Prefault::No,
    );
}

/// Decode `stp xzr, xzr, [Xn, #imm7*8]` signed-offset form.
/// Returns `(Rn_index, byte_offset)` or `None` if not this instruction form.
///
/// Encoding: opc=10 (64-bit), L=0 (store), V=0 (GPR), signed-offset class.
/// Mask 0xFFC07C1F clears imm7 [21:15] and Rn [9:5]; the constant 0xA9007C1F
/// matches Rt=11111 (xzr) and Rt2=11111 (xzr) with all other fixed bits set.
pub fn decode_stp_xzr_xzr(instr: u32) -> Option<(usize, i64)> {
    if (instr & 0xFFC0_7C1F) != 0xA900_7C1F {
        return None;
    }
    let rn = ((instr >> 5) & 0x1F) as usize;
    let imm7_raw = ((instr >> 15) & 0x7F) as i32;
    // Sign-extend 7-bit field to i32 via arithmetic shift.
    let imm7 = (imm7_raw << 25) >> 25;
    Some((rn, i64::from(imm7) * 8))
}

/// Human-readable syscall hint for forktest Pattern 2 serial (`GO_FORKTEST_DEBUG.md`).
fn syscall_nr_pattern2_hint(nr: u64) -> &'static str {
    use crate::syscall::nr;
    match nr {
        x if x == nr::READ => "read",
        x if x == nr::EPOLL_CTL => "epoll_ctl",
        x if x == nr::EPOLL_PWAIT => "epoll_pwait",
        x if x == nr::EPOLL_CREATE1 => "epoll_create1",
        x if x == nr::WRITE => "write",
        x if x == nr::MMAP => "mmap",
        x if x == nr::CLOSE => "close",
        x if x == nr::FCNTL => "fcntl",
        x if x == nr::IOCTL => "ioctl",
        x if x == nr::CLOCK_GETTIME => "clock_gettime",
        _ => "?",
    }
}

#[inline]
fn syscall_stub_elr_in_diag_window(elr: u64) -> bool {
    let min = crate::config::DEBUG_SIGSEGV_SYSCALL_STUB_ELIR_MIN;
    let max = crate::config::DEBUG_SIGSEGV_SYSCALL_STUB_ELIR_MAX;
    elr >= min && elr <= max
}

/// Forensics for the `N × INTERP_BASE + offset` instruction-abort class
/// (`docs/runbooks/debug-thread-spawn-segv.md`, class 2).
///
/// The branch target is read from a slot in the interpreter's writable data that
/// musl's **RELR** apply loop mutates with `*slot += base`. A slot that already
/// carried a relocated value gets `base` added a second time, so `FAR` reads
/// `N*base + link_time_offset` with `N` = how many times the slot was relocated.
/// That only happens if two processes' startup code ran over the **same physical
/// frame**, so the one fact worth capturing is the slot's **PA**: two faults
/// reporting the same PA means a shared frame, different PAs mean the poisoned
/// value travelled by copy (a CoW/page-cache fill from an already-bumped source).
///
/// Fires only on a `FAR` of that exact shape, and only walks the interpreter's
/// own VA window, so it costs nothing on any other abort.
fn interp_relr_forensics(far: usize, pid: u32) {
    const BASE: usize = akuma_exec::elf_loader::INTERP_BASE;
    // N >= 2 (N == 1 is a correctly relocated pointer) and a plausible in-image offset.
    let n = far / BASE;
    let off = far % BASE;
    if n < 2 || off >= 0x0010_0000 {
        return;
    }
    let ttbr0 = akuma_exec::mmu::get_current_ttbr0();
    if ttbr0 == 0 {
        return;
    }
    let l0_addr = ttbr0 & 0x0000_FFFF_FFFF_F000;
    let l0_ptr = akuma_exec::mmu::phys_to_virt(l0_addr) as *const u64;
    let tid = akuma_exec::threading::current_thread_id();
    let (cur_pid, ttbr0_proc, ppid) = akuma_exec::process::current_process_shared()
        .map_or((0, 0, 0), |p| (p.pid, p.address_space.ttbr0(), p.parent_pid));
    const L0_MASK: u64 = 0x0000_FFFF_FFFF_F000;
    let base_differs = ttbr0_proc != 0 && (ttbr0 as u64 & L0_MASK) != (ttbr0_proc & L0_MASK);
    crate::safe_print!(224,
        "[RELR] fault_pid={} cur_pid={} ppid={} tid={} N={} off={:#x}\n",
        pid, cur_pid, ppid, tid, n, off);
    crate::safe_print!(224,
        "[RELR] ttbr0_live={:#x} ttbr0_proc={:#x}{} expected_l0={:#x} switch_ins={} gen={}\n",
        ttbr0, ttbr0_proc,
        if base_differs { "  *** AS MISMATCH (foreign page tables) ***" } else { "" },
        akuma_exec::threading::thread_expected_l0(tid),
        akuma_exec::threading::thread_switch_ins(tid),
        akuma_exec::threading::thread_generation(tid));

    // The writable PT_LOAD of a musl interpreter sits at the top of its image;
    // 1 MB of VA covers it with room to spare and bounds the scan at 256 pages.
    let mut found = 0usize;
    for page in 0..256usize {
        let page_va = BASE + 0x0010_0000 - (page + 1) * 0x1000;
        let Some(pa) = akuma_exec::mmu::translate_user_va(l0_ptr, page_va) else {
            continue;
        };
        let kva = akuma_exec::mmu::phys_to_virt(pa & !0xFFF).cast::<u64>();
        for i in 0..512usize {
            let v = unsafe { kva.add(i).read_volatile() } as usize;
            if v == far {
                crate::safe_print!(160,
                    "[RELR] slot va={:#x} pa={:#x} val={:#x}\n",
                    page_va + i * 8, (pa & !0xFFF) + i * 8, v);
                found += 1;
                if found >= 4 {
                    return;
                }
            }
        }
    }
    if found == 0 {
        crate::safe_print!(128, "[RELR] no slot in interp window holds {:#x}\n", far);
    }
}

/// Log syscall number (**`x8`**), **FAR**, **pid/tid** when SIGSEGV hits the configured syscall-stub VA window.
fn maybe_print_sigsegv_syscall_diag(elr: u64, far: u64, frame: &UserTrapFrame) {
    if !crate::config::DEBUG_SIGSEGV_SYSCALL_STUB {
        return;
    }
    if !syscall_stub_elr_in_diag_window(elr) {
        return;
    }
    let nr = frame.x8;
    let hint = syscall_nr_pattern2_hint(nr);
    let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let tid = akuma_exec::threading::current_thread_id();
    crate::tprint!(
        384,
        "[sigsegv-syscall] pid={} tid={} FAR={:#x} ELR={:#x} x8={} ({}) x0={:#x} x1={:#x} x2={:#x} x3={:#x} x4={:#x} x5={:#x}\n",
        pid, tid, far, elr, nr, hint, frame.x0, frame.x1, frame.x2, frame.x3, frame.x4, frame.x5,
    );
}

/// Render an 8-byte little-endian word as the ASCII it may be hiding.
///
/// `docs/runbooks/debug-thread-spawn-segv.md`: a `FAR` that decodes to printable
/// text (`"libder-8"`, `"+outline"`, an ANSI SGR escape) is not a wild pointer —
/// it is a freed block that has been reused as string heap, and the string names
/// the allocator neighbour that took it. Two sessions have re-derived this by
/// hand; print it instead.
fn word_as_ascii(word: u64, out: &mut [u8; 8]) -> bool {
    let bytes = word.to_le_bytes();
    let mut printable = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        out[i] = match *b {
            0x20..=0x7e => { printable += 1; *b }
            0x1b => { printable += 1; b'^' }      // ESC — the ANSI-escape tell
            _ => b'.',
        };
    }
    printable >= 6
}

/// Diagnostics for the thread-spawn SIGSEGV class
/// (`docs/runbooks/debug-thread-spawn-segv.md`).
///
/// Printed on every fatal EL0 data abort, because the class is rare, unpredictable
/// and expensive to reproduce — one repro must answer the question, not motivate
/// another instrumented build. It reports the three facts the register dump alone
/// cannot distinguish between:
///
/// 1. **Address space.** `ttbr0_live` (the hardware register) against the
///    `Process`'s own `ttbr0`. A mismatch means the thread is executing in
///    someone else's page tables, which turns every "corrupt pointer" in the dump
///    into a correct pointer resolved in the wrong space.
/// 2. **The clone hand-off.** musl's `[entry, arg]` pair as the parent wrote it
///    versus as it reads *now*. Divergence indicts the memory handoff; agreement
///    exonerates it and points at the packet's lifetime.
/// 3. **ASCII.** `FAR` decoded as text — see `word_as_ascii`.
fn print_spawn_fault_diag(far: u64, frame: &UserTrapFrame) {
    let tid = akuma_exec::threading::current_thread_id();

    let ttbr0_live = akuma_exec::mmu::get_current_ttbr0() as u64;
    let ttbr0_proc = akuma_exec::process::current_process_shared()
        .map_or(0, |p| p.address_space.ttbr0());
    // Compare the L0 base (bits 47:0) and the ASID (bits 63:48) separately —
    // they mean very different things here. A CLONE_THREAD child is *supposed*
    // to run under a ttbr0 whose ASID differs from its own `Process`: it is
    // handed the parent's ttbr0 verbatim (`shared_ttbr0` in `clone_thread`)
    // while `new_shared` gives its Process a fresh ASID over the same L0. So
    // "ASID differs" is routine and must not be flagged. A differing **L0
    // base** is the real defect: the core is executing in a third party's page
    // tables, which is the cross-address-space aliasing theory (T1) in
    // docs/runbooks/debug-thread-spawn-segv.md §3c.
    const BASE: u64 = 0x0000_FFFF_FFFF_F000;
    let base_differs = ttbr0_proc != 0 && (ttbr0_live & BASE) != (ttbr0_proc & BASE);
    let asid_differs = ttbr0_proc != 0 && (ttbr0_live >> 48) != (ttbr0_proc >> 48);
    crate::safe_print!(224, "[Fault]  tid={} ttbr0_live={:#x} ttbr0_proc={:#x}{}\n",
        tid, ttbr0_live, ttbr0_proc,
        if base_differs {
            "  *** AS MISMATCH: L0 BASE DIFFERS (foreign page tables) ***"
        } else if asid_differs {
            "  (asid differs only — normal for a cloned thread)"
        } else { "" });
    // Discriminators for the AS MISMATCH stories (see thread_expected_l0 /
    // thread_switch_ins docs): expected==0 ⇒ the thread ran with the tripwire
    // baseline unset; switch_ins==0 ⇒ no scheduler restore since scrub, so the
    // live tables came ONLY from the first-entry path (activate → eret);
    // expected==proc && switch_ins>0 ⇒ every restore was checked and passed —
    // the corruption entered somewhere the switch path cannot see.
    crate::safe_print!(160,
        "[Fault]  expected_l0={:#x} switch_ins={} slot_gen={}\n",
        akuma_exec::threading::thread_expected_l0(tid),
        akuma_exec::threading::thread_switch_ins(tid),
        akuma_exec::threading::thread_generation(tid));

    let mut ascii = [0u8; 8];
    if word_as_ascii(far, &mut ascii)
        && let Ok(s) = core::str::from_utf8(&ascii) {
            crate::safe_print!(96, "[Fault]  FAR as ASCII: \"{}\" (freed block reused as string?)\n", s);
        }

    let Some(snap) = akuma_exec::process::clone_snapshot(tid) else { return };
    match akuma_exec::process::reread_clone_handoff(&snap) {
        Some((entry_now, arg_now)) => {
            // Do NOT flag a difference here as corruption. musl's `__clone` child
            // starts with `ldp x1, x0, [sp], #16` — it pops both handoff words and
            // hands the address straight back to the child as stack, so the child's
            // own first frame legitimately overwrites them within a few
            // instructions. These two lines are informational: the `at clone:`
            // values are the trustworthy ones (they say what the parent actually
            // handed over), and `now:` is only meaningful for a fault taken before
            // the child ever ran.
            crate::safe_print!(256,
                "[Fault]  clone-handoff tid={} stack={:#x} from pid={}/tid={} ttbr0={:#x}\n[Fault]    at clone: entry={:#x} arg={:#x}\n[Fault]    now:      entry={:#x} arg={:#x} (child frame reuses these; differing is normal)\n",
                tid, snap.stack, snap.parent_pid, snap.parent_tid, snap.ttbr0,
                snap.entry, snap.arg, entry_now, arg_now);
        }
        None => {
            crate::safe_print!(192,
                "[Fault]  clone-handoff tid={} stack={:#x} UNREADABLE now (was entry={:#x} arg={:#x})\n",
                tid, snap.stack, snap.entry, snap.arg);
        }
    }

    // The word the faulting prologue loaded came from `[x19]` (`ldr x20,[x0]`;
    // `mov x19,x0`). Dump that block so the reuse pattern — free-list cell, small
    // integer, or string — is on the record without a second run.
    let mut probe = [0u64; 4];
    let ok = akuma_exec::mmu::user_access::copy_from_user_with(
        akuma_exec::mmu::user_access::as_user_bytes_mut(&mut probe),
        frame.x19,
        akuma_exec::mmu::user_access::Prefault::No,
    )
    .is_ok();
    if ok {
        crate::safe_print!(192, "[Fault]  [x19={:#x}] = {:#x} {:#x} {:#x} {:#x}\n",
            frame.x19, probe[0], probe[1], probe[2], probe[3]);
        for w in probe {
            if word_as_ascii(w, &mut ascii)
                && let Ok(s) = core::str::from_utf8(&ascii) {
                    crate::safe_print!(96, "[Fault]    ascii: \"{}\"\n", s);
                }
        }
    } else {
        crate::safe_print!(96, "[Fault]  [x19={:#x}] unreadable\n", frame.x19);
    }
}

/// One-shot latch for the `[IA-PERM-UPGRADE]` line: the instruction-abort permission
/// arm runs per faulting page, and this path is common enough in principle
/// (every non-exec-mapped page a process executes) that a per-fault print would be a
/// log flood on the fault path. See the print site for what the line answers.
static IA_PERM_UPGRADE_SEEN: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Demand-page a lazy region, for **both** EL0 abort arms.
///
/// One body, two documented entry points ([`akuma_exec::mmu::FaultAccess`]) — the
/// shape §6 of `docs/archive/COW_PILE_AUDIT.md` prescribed for this merge. The data-
/// abort and instruction-abort arms ran two copies of this algorithm for a long time;
/// a `diff` of them was 378 lines of which everything load-bearing reduced to the
/// three parameters named below (§12 there records the merge and every difference it
/// resolved).
///
/// Three passes, and the split between them is the point:
///
/// - **Pass A — PLAN.** Count the pages that need a private frame, and resolve the
///   ones already available as shared read-only pages (`file_page_cache`) up front,
///   so the pool below covers real misses only. A fully-cached readahead batch must
///   not allocate — and immediately free — 256 frames per fault.
/// - **Pass B — FILL.** Read file data and run I-cache maintenance into PRIVATE
///   frames. This is the long part (block I/O) and runs with **no** `as_lock` and,
///   on `smp-shared`, with the BKL dropped, so peers can fault while this core waits
///   on the disk.
/// - **Pass C — INSTALL.** Map and track each filled frame under `as_lock`. Short, no
///   alloc, no I/O. A frame that loses the install race to a peer is freed after the
///   hold.
///
/// What the entry point decides, and nothing else:
///
/// 1. `map_flags` for a page whose region recorded none, and for every anonymous page
///    ([`akuma_exec::mmu::lazy_map_flags`]): `RW_NO_EXEC` for a load/store, `RX` for
///    an instruction fetch.
/// 2. The log tag (`[DA-DP]` / `[IA-DP]`), because the archive greps for both.
///
/// Everything else — including whether the frame needs I-cache maintenance — follows
/// from `map_flags`, which is why `is_exec` is derived here rather than passed in.
///
/// Returns `true` when the faulting page is present on return (mapped by this call or
/// by a peer that won the race), i.e. when the caller should return to EL0 and let the
/// access retry. `false` means every repair failed — the caller falls through to its
/// own diagnostics and SIGSEGV. The fault slot is held for the whole call and released
/// on every exit path, including that one.
#[allow(clippy::too_many_arguments)]
fn demand_page_lazy_region(
    access: akuma_exec::mmu::FaultAccess,
    pid: u32,
    as_owner: u32,
    far_usize: usize,
    flags: u64,
    source: &akuma_exec::process::LazySource,
    region_start: usize,
    region_size: usize,
) -> bool {
    let page_va = far_usize & !(0xFFF);

    // Serialize demand paging per-page to prevent races when multiple CLONE_VM
    // threads fault on the same page. Holder-tracked so a sibling can reclaim the
    // slot if the holder died mid-fault (see fault_slot_acquire). The guard releases
    // on ALL exit paths from this function, including the early returns and the
    // fall-through to the caller's SIGSEGV.
    let _fault_guard = fault_slot_hold(pid, as_owner, page_va);

    let file_backed = matches!(source, akuma_exec::process::LazySource::File { .. });
    let map_flags = akuma_exec::mmu::lazy_map_flags(access, flags, file_backed);
    // I-cache maintenance is a property of the *mapping* this fault installs, not of
    // the arm the fault arrived through: a page mapped non-exec cannot be fetched
    // from, so `dc cvau`/`ic ivau` over it buys nothing. The instruction-abort arm
    // used to hardcode `true` here — see COW_PILE_AUDIT.md §12.1 for why that was a
    // cost rather than a correctness requirement, and what pays for it now.
    let is_exec = akuma_exec::mmu::user_flags::is_exec(map_flags);
    if access == akuma_exec::mmu::FaultAccess::Instruction && !is_exec {
        // An instruction fetch into a mapping its own region records as non-exec.
        // Counted because nothing in this tree had ever measured whether it happens:
        // the page maps non-exec (it always did — `map_flags` never consulted the
        // arm), so the fetch cannot succeed until the permission-fault arm upgrades
        // it to RX, which is where its I-cache maintenance now happens.
        crate::pmm::dp_count(&crate::pmm::DP_IA_NOEXEC_FAULTS, 1);
    }
    // Hoisted out of the per-page readahead loops below: `map_flags` is
    // loop-invariant, and the predicate now reads the registered `ExecConfig` (which
    // `config()` returns **by value** — the whole struct), so evaluating it per page
    // cost a ~45-field copy up to 512 times per fault. Once per fault instead.
    let shareable_mapping = crate::file_page_cache::is_shareable_mapping(map_flags);

    if let akuma_exec::process::LazySource::File {
        ref path, inode, mount_id, file_offset, filesz, segment_va, ..
    } = *source
    {
        if crate::config::DEMAND_PAGE_LOG_ENABLED {
            crate::tprint!(256, "[{}] file region: fault_va={:#x} seg_va={:#x} filesz={:#x} file_off={:#x}\n",
                access.tag(), far_usize, segment_va, filesz, file_offset);
        }
        const READAHEAD_PAGES: usize = 256;
        let region_end = region_start + region_size;
        let ra_end = core::cmp::min(page_va + READAHEAD_PAGES * 0x1000, region_end);

        // Pass A — PLAN (see the header). Each shared hit already holds a reference
        // for this mapper; anything that never gets installed is freed with the
        // race-lost frames below, which balances it. Executable text is the main
        // beneficiary — this is the path four concurrent `rustc`s take through the
        // same `librustc_driver.so`.
        // `(va, frame, needs_icache, owns_ref)`. `owns_ref` records whether this
        // fault still holds a global reference for the frame; `adopt_user_frame`
        // consumes it on install, and Pass C frees it on a lost race. Pool frames
        // always own theirs; shared hits own the one `lookup_and_ref` took.
        let mut shared: alloc::vec::Vec<(usize, crate::pmm::PhysFrame, bool, bool)> =
            alloc::vec::Vec::new();
        let mut needed = 0usize;
        {
            let mut va = page_va;
            while va < ra_end {
                if !akuma_exec::mmu::is_current_user_page_mapped(va) {
                    // Only whole pages fully covered by file data are shareable: a
                    // page straddling `filesz` has a zero-fill tail whose length
                    // belongs to the mapping, not the file, so two mappers can
                    // legitimately disagree about its contents.
                    let full = va >= segment_va && va + 0x1000 <= segment_va + filesz;
                    let hit = if full && shareable_mapping {
                        let file_off = file_offset + (va - segment_va);
                        crate::file_page_cache::lookup_and_ref(mount_id, inode, file_off, is_exec)
                    } else {
                        None
                    };
                    match hit {
                        Some((pf, needs_ic)) => {
                            // The reference `lookup_and_ref` took is kept until Pass C
                            // decides this frame's fate — it is what keeps the cache
                            // entry's frame alive across the fill. `adopt_user_frame`
                            // consumes it, and reports back if it turned out surplus.
                            // Reconciling here (the old `drop_surplus_shared_ref`) split
                            // the two updates across the `as_lock` hold and could drive
                            // the count below the truth on a lost install race.
                            shared.push((va, pf, needs_ic, true));
                        }
                        None => needed += 1,
                    }
                }
                va += 0x1000;
            }
        }
        let mut sh_idx = 0usize;

        // Clamp the readahead batch so file-backed demand paging never drains the PMM
        // below USER_PAGE_RESERVE — the same floor the anonymous path respects via
        // alloc_page_zeroed_user(). Without this an mmap larger than RAM drains PMM to
        // 0, and a later kernel-side alloc (IRQ/scheduler, no current process) panics
        // into a whole-kernel brk #1 abort instead of SIGSEGV-ing the offending
        // process. When the budget hits 0 (free <= reserve) nothing maps and we fall
        // through to the single-page fallback below (alloc_page_zeroed_user -> None ->
        // SIGSEGV).
        let needed = needed.min(crate::pmm::user_readahead_budget(crate::pmm::free_count()));

        // Batch-allocate all needed frames in one lock acquisition
        let frame_pool = if needed > 0 {
            crate::pmm::alloc_pages_zeroed(needed).unwrap_or_else(|| {
                // Fallback: allocate what we can one at a time, still honouring the
                // reserve so we can't starve the kernel.
                let mut v = alloc::vec::Vec::new();
                for _ in 0..needed {
                    match crate::pmm::alloc_page_zeroed_user() {
                        Some(f) => v.push(f),
                        None => break,
                    }
                }
                v
            })
        } else {
            alloc::vec::Vec::new()
        };
        let mut pool_idx = 0usize;

        // Pass B — FILL (no `as_lock`, no BKL-critical state): read file data + do
        // icache maintenance into PRIVATE pool frames. This is the long part (block
        // I/O) and runs OUTSIDE the per-AS lock so peers can fault/run in parallel
        // (M5b). Records (va, frame) to install next.
        // `(va, frame, owns_ref)` — see `shared` above. Pool frames always carry a
        // reference of their own; shared hits may not.
        let mut filled: alloc::vec::Vec<(usize, crate::pmm::PhysFrame, bool)> =
            alloc::vec::Vec::new();
        // M5b Stage 4a: DROP the BKL for the block-I/O fill so peer cores can enter
        // the kernel while this core waits on the disk (the measured ~10 ms hold).
        // Pass B touches only PRIVATE frames + the block device (own lock) + the held
        // fault-slot — no BKL-protected state. The dropped-window ledger keeps the
        // fill BKL-free across timer ticks (the IRQ reconcile used to re-hold it for
        // the fill's remainder); the wrapper's leave_kernel still balances it.
        // Concurrent munmap clears PTEs but never frees the intermediate tables this
        // loop's page-table reads walk (freed only at teardown, which can't run while
        // this thread faults).
        #[cfg(kernel_smp_shared)]
        let fault_dropped_bkl = crate::smp_shared::fault_bkl_drop_enabled();
        #[cfg(kernel_smp_shared)]
        if fault_dropped_bkl { akuma_exec::bkl::dropped_window_open(); }
        let mut cur_va = page_va;
        while cur_va < ra_end {
            if akuma_exec::mmu::is_current_user_page_mapped(cur_va) {
                cur_va += 0x1000;
                continue;
            }
            // Shared hit resolved in Pass A: already filled by whoever faulted it
            // first, so there is nothing to read and no frame to consume. `shared` is
            // built in ascending VA order by the same walk this loop performs, so an
            // index pointer is enough.
            if sh_idx < shared.len() && shared[sh_idx].0 == cur_va {
                let (_, pf, needs_ic, owns_ref) = shared[sh_idx];
                sh_idx += 1;
                // Diagnostic (config::FPCACHE_VERIFY_HITS, off by default): re-read this
                // page from disk and compare against what the cache handed us. This is
                // the direct test for "the cache is serving bytes that are not the
                // file's" — the alternative is inferring it from a userspace symptom
                // three layers up. See `pmm::DP_FILE_CACHE_MISMATCH`.
                if crate::config::FPCACHE_VERIFY_HITS {
                    let file_off = file_offset + (cur_va - segment_va);
                    let mut disk = alloc::vec![0u8; 0x1000];
                    let got = if inode != 0 {
                        crate::vfs::read_at_by_inode(path, inode, file_off, &mut disk)
                    } else {
                        crate::vfs::read_at(path, file_off, &mut disk)
                    };
                    let kva = akuma_exec::mmu::phys_to_virt(pf.addr).cast::<u8>();
                    let cached = unsafe { core::slice::from_raw_parts(kva, 0x1000) };
                    if got == Ok(0x1000) && cached != disk.as_slice() {
                        let at = cached.iter().zip(disk.iter()).position(|(a, b)| a != b).unwrap_or(0);
                        crate::pmm::dp_count(&crate::pmm::DP_FILE_CACHE_MISMATCH, 1);
                        crate::tprint!(256,
                            "[FPC-BAD] pid={} inode={} file_off={:#x} va={:#x} first_diff={:#x} cached={:#x} disk={:#x} cached_zero={}\n",
                            pid, inode, file_off, cur_va, at, cached[at], disk[at],
                            u8::from(cached.iter().all(|b| *b == 0)));
                    }
                }
                if needs_ic {
                    // Cached from a plain RO mapper that never ran `ic ivau`; this
                    // mapper wants to execute it, so pay the maintenance once and
                    // record it for the next mapper. `sync_icache_range` returns only
                    // after the closing `dsb ish`, so the invalidation is complete and
                    // inner-shareable BEFORE `mark_icache_clean` tells the next mapper
                    // it may skip the maintenance.
                    let kva = akuma_exec::mmu::phys_to_virt(pf.addr) as usize;
                    akuma_exec::mmu::sync_icache_range(kva, akuma_exec::mmu::PAGE_SIZE);
                    let file_off = file_offset + (cur_va - segment_va);
                    crate::file_page_cache::mark_icache_clean(mount_id, inode, file_off, pf);
                }
                filled.push((cur_va, pf, owns_ref));
                cur_va += 0x1000;
                continue;
            }
            if pool_idx >= frame_pool.len() {
                break;
            }
            let pf = frame_pool[pool_idx];
            pool_idx += 1;

            // `true` unless the fill below came up short. A page with no file data to
            // read (entirely past `filesz`) is trivially complete: its zeros are the
            // mapping's own zero-fill tail, not a failed read.
            let mut fill_complete = true;
            {
                let pg_data_start = core::cmp::max(cur_va, segment_va);
                let pg_data_end = core::cmp::min(cur_va + 0x1000, segment_va + filesz);
                if pg_data_start < pg_data_end {
                    let dst_off = pg_data_start - cur_va;
                    let file_off = file_offset + (pg_data_start - segment_va);
                    let len = pg_data_end - pg_data_start;
                    let page_ptr = akuma_exec::mmu::phys_to_virt(pf.addr);
                    let page_buf = unsafe {
                        core::slice::from_raw_parts_mut(page_ptr.cast::<u8>().add(dst_off), len)
                    };
                    let got = if inode != 0 {
                        crate::vfs::read_at_by_inode(path, inode, file_off, page_buf)
                    } else {
                        crate::vfs::read_at(path, file_off, page_buf)
                    };
                    // The range is already clamped to `filesz`, so anything less than
                    // `len` is a defect, not EOF. The frame came from
                    // `alloc_pages_zeroed`, so whatever was not read reads back as
                    // zeros — indistinguishable from real file content unless we say so
                    // here. See `pmm::DP_FILE_FILL_SHORT`.
                    fill_complete = got == Ok(len);
                    if !fill_complete {
                        crate::pmm::dp_count(&crate::pmm::DP_FILE_FILL_SHORT, 1);
                        crate::tprint!(288,
                            "[FILL-SHORT] pid={} inode={} file_off={:#x} want={} got={:?} va={:#x} path={} — page left zero-filled\n",
                            pid, inode, file_off, len, got, cur_va, path);
                    }
                }
            }

            if is_exec {
                // By the kernel VA (kva), not the user VA: the user page is not mapped
                // yet, so `ic ivau` on cur_va translation-faults on real hardware /
                // HVF. I-cache invalidation to PoU is by physical address, so the kva
                // alias of the same frame is equivalent and always mapped.
                let kva = akuma_exec::mmu::phys_to_virt(pf.addr) as usize;
                akuma_exec::mmu::sync_icache_range(kva, akuma_exec::mmu::PAGE_SIZE);
            }
            // Publish for every other mapper of this file page. Must come after the
            // fill and the I-cache maintenance above — and after the maintenance has
            // *completed*, which is why the closing `dsb ish` lives inside
            // `sync_icache_range` rather than after the install pass: a peer core can
            // map this frame the instant it lands in the cache, and it will trust
            // `icache_done`.
            //
            // `icache_done: is_exec` is a claim about the FRAME ("has been through
            // `dc cvau` + `ic ivau`"), not about this mapping's permissions, and it is
            // exactly as true as the gate above: `false` means "maintain it yourself",
            // which is the safe direction for a later `RX` mapper. See
            // COW_PILE_AUDIT.md F5 and §12.1.
            //
            // `fill_complete` is the load-bearing addition: publishing a frame whose
            // fill came up short turns one transient read failure into a permanent
            // `(inode, file_off)` entry full of zeros, which every later mapper takes
            // as a *hit* and never re-reads. The frame stays private and correct-ish
            // for this mapper (it is what the read produced); it just does not get to
            // speak for every other process.
            if cur_va >= segment_va
                && cur_va + 0x1000 <= segment_va + filesz
                && shareable_mapping
            {
                if fill_complete {
                    let file_off = file_offset + (cur_va - segment_va);
                    crate::file_page_cache::insert(mount_id, inode, file_off, pf, is_exec);
                } else {
                    crate::pmm::dp_count(&crate::pmm::DP_FILE_FILL_UNPUBLISHED, 1);
                }
            }
            filled.push((cur_va, pf, true));
            cur_va += 0x1000;
        }
        // Close the window: re-takes the BKL for the install pass (unless a
        // still-open outer window means it must stay dropped).
        #[cfg(kernel_smp_shared)]
        if fault_dropped_bkl { akuma_exec::bkl::dropped_window_close(); }

        // Pass C — INSTALL (under `as_lock`): atomically map each filled frame +
        // track it. Short, no alloc/IO. A frame that loses the install race (a peer
        // mapped the VA) is collected and freed after the hold.
        let mut any_mapped = false;
        let mut pages_mapped = 0u64;
        let mut race_free: alloc::vec::Vec<crate::pmm::PhysFrame> = alloc::vec::Vec::new();
        if let Some(owner) = akuma_exec::process::lookup_process_shared(as_owner) {
            #[cfg(kernel_smp_shared)]
            let _asg = akuma_exec::process::AsLockHold::new(&owner.as_lock);
            for (cur_va, pf, owns_ref) in filled.iter().copied() {
                // no_flush: batch the TLB invalidation after the loop.
                let (table_frames, installed) = unsafe {
                    akuma_exec::mmu::map_user_page_no_flush(cur_va, pf.addr, map_flags)
                };
                for tf in table_frames {
                    owner.address_space.track_page_table_frame(tf);
                }
                if installed {
                    // One hold maintains both the per-AS frame list and the global
                    // share count, and hands back any surplus reference rather than
                    // leaving it to a separate reconciliation pass.
                    if owner.address_space.adopt_user_frame(pf, owns_ref) {
                        race_free.push(pf);
                    }
                    any_mapped = true;
                    pages_mapped += 1;
                } else {
                    // Race: a peer mapped this page. Release our reference after the
                    // hold — but ONLY if this fault still holds one. Freeing a frame
                    // whose reference was already balanced drove `cow_ref` one below
                    // the truth, handing a live frame back to the PMM to be recycled
                    // and re-zeroed under its remaining mappers
                    // (docs/archive/SELFHOST_ZERO_PAGE_HUNT.md §6).
                    if owns_ref {
                        race_free.push(pf);
                    }
                    if cur_va == page_va {
                        any_mapped = true;
                    }
                }
            }
            // Flush the whole mapped run in one shot (fewer barriers).
            if any_mapped {
                akuma_exec::mmu::flush_tlb_range(page_va, pages_mapped as usize);
            }
        } else {
            // No owner (degenerate): nothing to install; free what we filled.
            for (_, pf, owns_ref) in filled.iter().copied() {
                if owns_ref { race_free.push(pf); }
            }
        }

        // Free race-lost frames + unused pool frames (outside the hold).
        for pf in race_free { crate::pmm::free_page_at(pf, akuma_pmm::FreeSite::FaultRaceLost); }
        while pool_idx < frame_pool.len() {
            crate::pmm::free_page_at(frame_pool[pool_idx], akuma_pmm::FreeSite::FaultPoolSurplus);
            pool_idx += 1;
        }

        // No `dsb ish; isb` here. It used to sit at this point, *after* the install
        // pass, which put the completion of the I-cache maintenance later than the
        // first instant a peer could fetch from the frame (Pass B publishes to
        // `file_page_cache`; Pass C publishes the PTE). The pair now closes each
        // page's `sync_icache_range` in Pass B, and `flush_tlb_range` above ends in
        // `dsb ish; isb` of its own, so the return to EL0 is still fully
        // synchronized. See COW_PILE_AUDIT.md F4.

        if any_mapped {
            crate::pmm::dp_count(&crate::pmm::DP_FILE_PAGES, pages_mapped as usize);
            crate::syscall::syscall_counters::inc_pagefault(pages_mapped);
            if crate::config::PROCESS_SYSCALL_STATS
                && let Some(owner) = akuma_exec::process::lookup_process_shared(as_owner) {
                    owner.syscall_stats.inc_pagefault(pages_mapped);
                }
            return true;
        } else if akuma_exec::mmu::is_current_user_page_mapped(page_va) {
            // Race: another CPU mapped the faulting page while we were doing
            // readahead. The page is now present — return success.
            return true;
        }
        // Readahead pool was exhausted before reaching page_va.
        // Fall back to a single-page allocation for just the faulting page.
        let (_, _, free) = crate::pmm::stats();
        crate::tprint!(160, "[{}] pid={} va=0x{:x} readahead pool exhausted, {} free pages — retrying single page\n",
            access.tag(), pid, far_usize, free);
        if let Some(pf) = crate::pmm::alloc_page_zeroed_user() {
            // Re-read file data for this single page
            let pg_data_start = core::cmp::max(page_va, segment_va);
            let pg_data_end = core::cmp::min(page_va + 0x1000, segment_va + filesz);
            if pg_data_start < pg_data_end {
                let dst_off = pg_data_start - page_va;
                let file_off = file_offset + (pg_data_start - segment_va);
                let len = pg_data_end - pg_data_start;
                let page_ptr = akuma_exec::mmu::phys_to_virt(pf.addr);
                let page_buf = unsafe {
                    core::slice::from_raw_parts_mut(page_ptr.cast::<u8>().add(dst_off), len)
                };
                let got = if inode != 0 {
                    crate::vfs::read_at_by_inode(path, inode, file_off, page_buf)
                } else {
                    crate::vfs::read_at(path, file_off, page_buf)
                };
                // This arm never publishes to `file_page_cache`, so a short read here
                // poisons only the faulting process — but it is the same defect and
                // shares the counter, so a build that trips one can be told from a
                // build that trips the other by the `[FILL-SHORT]` tag alone.
                if got != Ok(len) {
                    crate::pmm::dp_count(&crate::pmm::DP_FILE_FILL_SHORT, 1);
                    crate::tprint!(288,
                        "[FILL-SHORT/single] pid={} inode={} file_off={:#x} want={} got={:?} va={:#x} path={}\n",
                        pid, inode, file_off, len, got, page_va, path);
                }
            }
            if is_exec {
                // By the kernel VA (kva), not the user VA: the user page is not mapped
                // yet at this point (the map happens below), so `ic ivau` on page_va
                // translation-faults on real hardware / HVF. I-cache invalidation to
                // PoU is by physical address, so the kva alias of the same frame is
                // equivalent and always mapped. The sequence completes (`dsb ish;
                // isb`) before the PTE below publishes the frame.
                let kva = akuma_exec::mmu::phys_to_virt(pf.addr) as usize;
                akuma_exec::mmu::sync_icache_range(kva, akuma_exec::mmu::PAGE_SIZE);
            }
            // Install + track under `as_lock` (frame + file data already prepared
            // above, outside the hold).
            let mut installed_ok = false;
            if let Some(owner) = akuma_exec::process::lookup_process_shared(as_owner) {
                #[cfg(kernel_smp_shared)]
                let _asg = akuma_exec::process::AsLockHold::new(&owner.as_lock);
                let (table_frames, installed) = unsafe {
                    akuma_exec::mmu::map_user_page(page_va, pf.addr, map_flags)
                };
                for tf in table_frames { owner.address_space.track_page_table_frame(tf); }
                if installed {
                    owner.address_space.track_user_frame(pf);
                    installed_ok = true;
                }
            }
            if !installed_ok {
                crate::pmm::free_page(pf);
            }
            crate::pmm::dp_count(&crate::pmm::DP_FILE_PAGES, 1);
            crate::syscall::syscall_counters::inc_pagefault(1);
            return true;
        }
        let (_, _, free2) = crate::pmm::stats();
        crate::tprint!(160, "[{}] pid={} va=0x{:x} single-page fallback OOM, {} free pages\n",
            access.tag(), pid, far_usize, free2);
    } else if let Some(page_frame) = crate::pmm::alloc_page_zeroed_user() {
        // Anonymous demand page: frame (zeroed) allocated OUTSIDE `as_lock`;
        // install + track under it.
        let mut installed_ok = false;
        if let Some(owner) = akuma_exec::process::lookup_process_shared(as_owner) {
            #[cfg(kernel_smp_shared)]
            let _asg = akuma_exec::process::AsLockHold::new(&owner.as_lock);
            let (table_frames, installed) = unsafe {
                akuma_exec::mmu::map_user_page(page_va, page_frame.addr, map_flags)
            };
            for tf in table_frames {
                owner.address_space.track_page_table_frame(tf);
            }
            if installed {
                owner.address_space.track_user_frame(page_frame);
                installed_ok = true;
            }
        }
        if installed_ok {
            crate::pmm::dp_count(&crate::pmm::DP_ANON_PAGES, 1);
            crate::syscall::syscall_counters::inc_pagefault(1);
            if crate::config::PROCESS_SYSCALL_STATS
                && let Some(owner) = akuma_exec::process::lookup_process_shared(as_owner) {
                    owner.syscall_stats.inc_pagefault(1);
                }
        } else {
            // Race (peer mapped it) or no owner: free our frame.
            crate::pmm::free_page(page_frame);
        }
        // Page is mapped (by us or another CPU) - success
        return true;
    } else {
        let (_, _, free) = crate::pmm::stats();
        crate::tprint!(160, "[{}] pid={} va=0x{:x} anon alloc failed, {} free pages\n",
            access.tag(), pid, far_usize, free);
    }
    false
}

/// Which mechanism overwrites the PTE during a CoW break, and who holds `as_lock`
/// while it happens.
///
/// This is the one *genuine* disagreement between the three CoW-break sites, so it
/// stays a parameter instead of becoming a decision made inside the helper.
///
/// **It would not collapse even once F1 lands** (an earlier revision of this comment
/// predicted it would). The two variants also differ in who *acquires* the lock and in
/// how the PTE is overwritten — `aspace.map_page` through `&mut UserAddressSpace` versus
/// `mmu::remap_current_user_page` through the live TTBR0. Unifying those is a change to
/// the PTE-write mechanism on the fault path, which is not what F1 asked for.
pub enum CowRemap<'a> {
    /// The EL1 paths ([`ensure_cow_page_writable`] and [`try_resolve_el1_cow_fault`]):
    /// take `as_lock` via `with_address_space` for the copy, the PTE overwrite and
    /// the frame bookkeeping (F1, applied 2026-08-14 — see
    /// [`copy_page_under_as_lock`]).
    TakingAsLock(&'a akuma_exec::process::Process),
    /// The EL0 fault path: the caller already holds `as_lock` (`AsLockHold`) across
    /// the whole break, so the helper must not take it again — and it remaps through
    /// the live TTBR0 rather than through `&mut UserAddressSpace`.
    CallerHoldsAsLock(&'a akuma_exec::process::Process),
}

/// Copy a 4 KiB page between two physical frames — the CoW break's payload.
///
/// **Both arms call this under `as_lock` (F1, applied 2026-08-14).** The hold is what
/// makes `old_pa` valid for the duration of the read: without it a peer core's `munmap`
/// or CoW break can free `old_pa` mid-copy, and the copy then reads a frame that is
/// back on the PMM free list — picking up quarantine poison
/// (`0xFEEDFACEDEAD0000 ^ pa`) or a recycled frame's contents. That is the signature
/// class of the open `CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md` defect.
///
/// # Why F1 was held back until 2026-08-14
///
/// Moving this call inside `with_address_space` for the EL1 arm — §8 row 6's exact
/// prescription — was first implemented and backed out on 2026-08-13: with it applied,
/// the SMP=1 exercise suite wedged in ~3 of 4 runs. F1 was never the defect — it was an
/// *amplifier* for F8 (`COW_PILE_AUDIT.md` §10): the scheduler SGI could install a
/// freed L0 from a zombie's saved context, and F1 widened the preempted-mid-exit window
/// that arms that race. With the saved-context free gate landed, 10 of 10 amplified
/// suite runs and 3 of 3 unamplified ones were clean, so the copy moved inside for good.
///
/// It is a function so the requirement has one statement instead of three, and so the
/// two `CowRemap` arms cannot drift on it silently.
#[inline]
fn copy_page_under_as_lock(old_pa: usize, new_frame: crate::pmm::PhysFrame) {
    // SAFETY: `old_pa` came from a valid user PTE and `new_frame` from the PMM, so
    // both are identity-mapped, page-aligned and 4 KiB wide; the regions cannot
    // overlap because `new_frame` was just allocated and is not mapped anywhere yet.
    unsafe {
        let src = akuma_exec::mmu::phys_to_virt(old_pa).cast_const();
        let dst = akuma_exec::mmu::phys_to_virt(new_frame.addr);
        core::ptr::copy_nonoverlapping::<u8>(src, dst, 0x1000);
    }
}

/// Finish a CoW break: private copy of `old_pa` into `new_frame`, the faulting VA
/// remapped RW to the copy, and the old frame's **global** CoW reference dropped
/// only if this address space just gave up its last VA on it.
///
/// This is the shared middle of all three CoW-break paths — the EL0 permission
/// fault, the EL1 pre-flight [`ensure_cow_page_writable`], and the EL1 data abort
/// [`try_resolve_el1_cow_fault`]. Their *entry* conditions are deliberately not
/// shared and stay at the call sites (`COW_PILE_AUDIT.md` §8): the EL0 path needs the
/// stale-fault absorb and the per-page fault slot because it can race a sibling on
/// the same page, while the EL1 pre-flight is called *before* a kernel write and
/// must be able to answer "no CoW page here, proceed" without touching a lock.
/// Owner resolution likewise stays at the call site, but all three sites now resolve
/// the **address-space owner** (F2, fixed 2026-08-13).
///
/// **F1b (closed 2026-08-14).** The two EL1 sites still *translate* the VA and read
/// `cow_ref_get` **outside** the hold — that cannot move (the sites answer "is this
/// even a CoW page?" before taking any lock, and the alloc must stay outside the
/// hold) — so the `TakingAsLock` arm **re-validates both under the hold** and
/// declines the break (frees `new_frame`, changes nothing) if a peer got there
/// first. The EL0 arm needs no re-validation: it holds `as_lock` across translate,
/// refcount and copy alike. See `COW_PILE_AUDIT.md` §4 F1b, option 1.
///
/// The reason this middle is worth one definition is the `released_last_va` gate
/// below. `COW_REFCOUNTS` counts **address spaces** — the first share inserts 2,
/// "parent + child" — while `user_frames` counts **VAs**, which is why
/// `remove_user_frame` reports the last one. Decrementing per *VA broken* drops a
/// reference the address space is still using, and the next holder's decrement then
/// frees a frame we still map. That is the §5.6 refcount underflow, and it was fixed
/// three times because this code existed in three places.
pub fn complete_cow_break(
    remap: CowRemap<'_>,
    page_va: usize,
    old_pa: usize,
    new_frame: crate::pmm::PhysFrame,
) {
    crate::pmm::track_frame(new_frame, akuma_exec::runtime::FrameSource::UserData);

    let released_last_va = match remap {
        CowRemap::TakingAsLock(owner) => {
            let outcome = owner.with_address_space(|aspace| {
                // F1b (COW_PILE_AUDIT.md §4, closed 2026-08-14): the EL1 callers
                // translated `page_va` and read the refcount BEFORE this hold began,
                // so a peer's munmap or CoW break may have retired `old_pa` in
                // between — the copy below would then read a freed (quarantine-
                // poisoned or recycled) frame. Re-validate both entry conditions
                // under the hold; `None` declines the break. This is the same
                // invariant the EL0 arm gets by construction (it pre-holds
                // `as_lock` across translate, refcount and copy alike).
                let still_named =
                    aspace.translate(page_va).map(|pa| pa & !0xFFF) == Some(old_pa);
                if !still_named || crate::pmm::cow_ref_get(old_pa) == 0 {
                    return None;
                }
                // F1 (§8 row 6): the copy runs INSIDE the `as_lock` hold, so
                // `old_pa` cannot be freed by a peer mid-copy. Held back until
                // 2026-08-14 because it amplified the F8 wedge (§10.2); with the
                // saved-context free gate landed, 10/10 amplified suite runs were
                // clean and this is safe.
                copy_page_under_as_lock(old_pa, new_frame);
                let _ = aspace.map_page(
                    page_va, new_frame.addr, akuma_exec::mmu::user_flags::RW_NO_EXEC,
                );
                akuma_exec::mmu::flush_tlb_page(page_va);
                aspace.track_user_frame(new_frame);
                // CoW: the old page is freed via the global CoW refcount, never here.
                Some(aspace.remove_user_frame(akuma_exec::runtime::PhysFrame::new(old_pa)))
            });
            let Some(released) = outcome else {
                // Declined: a peer resolved this page first (its break made the
                // PTE name a private frame, or a munmap removed it). Leave the
                // mapping exactly as the peer left it and return the unused
                // frame. Callers proceed/retry: a completed sibling break means
                // the retried access succeeds; a genuine unmap re-faults with a
                // translation (not permission) code and takes its normal path.
                crate::pmm::free_page(new_frame);
                return;
            };
            released
        }
        CowRemap::CallerHoldsAsLock(owner) => {
            // The caller's `AsLockHold` already covers this copy.
            copy_page_under_as_lock(old_pa, new_frame);
            // Overwrite PTE: same VA, new PA, RW (free fn → shared `&Process`;
            // `map_user_page` would refuse the valid PTE).
            akuma_exec::mmu::remap_current_user_page(
                page_va, new_frame.addr, akuma_exec::mmu::user_flags::RW_NO_EXEC,
            );
            owner.address_space.track_user_frame(new_frame);
            // CoW: the old page is freed via the global CoW refcount, never here.
            owner.address_space
                .remove_user_frame(akuma_exec::runtime::PhysFrame::new(old_pa))
        }
    };

    // Only this address space's LAST VA on the frame gives up its global reference.
    //
    // **This gate is what makes the cross-process CoW break safe**, and it is the
    // only thing that does. A deleted `cow_fault_lock` used to sit at the EL0 site
    // claiming that job: a per-PA counter, incremented and decremented around the
    // break, that nothing ever read and nothing ever waited on — so it excluded no
    // one (`COW_PILE_AUDIT.md` §5). Two address spaces breaking the same frame
    // concurrently is safe because each frees at most one global reference and only
    // for its own last VA, not because they take turns. Making that counter into a
    // real lock would put a cross-core serialization point on the hottest path in
    // the kernel to re-solve a solved problem.
    if released_last_va {
        crate::pmm::cow_ref_dec(old_pa);
    }
}

fn ensure_cow_page_writable(pid: u32, page_va: usize) -> bool {
    let as_owner = akuma_exec::process::address_space_owner_pid_for_fault().unwrap_or(pid);
    let ttbr0: u64;
    #[cfg(target_os = "none")]
    unsafe { core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr0); }
    #[cfg(not(target_os = "none"))]
    { ttbr0 = 0; }
    if ttbr0 == 0 { return true; } // no user page tables — no CoW pages possible

    let l0_addr = (ttbr0 & 0x0000_FFFF_FFFF_F000) as usize;
    let l0_ptr = akuma_exec::mmu::phys_to_virt(l0_addr) as *const u64;

    let old_pa = match akuma_exec::mmu::translate_user_va(l0_ptr, page_va) {
        Some(pa) => pa & !0xFFF,
        None => return true, // page not mapped — not a CoW page
    };

    if crate::pmm::cow_ref_get(old_pa) == 0 { return true; } // not CoW, already writable

    // CoW page: allocate a private copy and remap as RW.
    let new_frame = match crate::pmm::alloc_page_zeroed() {
        Some(f) => f,
        None => return false, // OOM
    };
    // Owner resolution is the call site's business (F2): this path resolves the
    // *address-space owner*, which is the correct pid for a `CLONE_VM` sibling.
    // `try_resolve_el1_cow_fault` uses `read_current_pid()` and is wrong.
    if let Some(owner) = akuma_exec::process::lookup_process_shared(as_owner) {
        // `TakingAsLock`: the copy runs outside `as_lock`, the PTE remap and frame
        // bookkeeping inside it, excluding a concurrent BKL-free fault on this
        // address space. See `CowRemap` for why that split is preserved as-is.
        complete_cow_break(CowRemap::TakingAsLock(owner), page_va, old_pa, new_frame);
        true
    } else {
        crate::pmm::free_page(new_frame); // no owner — free to avoid leak
        false
    }
}

/// Ensure a userspace page is mapped. If it's in a lazy anonymous region and
/// not yet mapped, allocates and maps a zeroed page. Returns true if the page
/// is mapped after this call (either was already mapped, or was just demand-paged).
fn ensure_user_page_mapped(pid: u32, page_va: usize) -> bool {
    let as_owner = akuma_exec::process::address_space_owner_pid_for_fault().unwrap_or(pid);
    if akuma_exec::mmu::is_current_user_page_mapped(page_va) {
        return true;
    }
    // Check if the page is in a lazy anonymous region
    if let Some((flags, source, _region_start, _region_size)) =
        akuma_exec::process::lazy_region_lookup_for_page_fault(pid, page_va)
    {
        // Only demand-page anonymous regions here; file-backed pages handled by the fault path
        // PROT_NONE regions must NOT be demand-paged — access should SIGSEGV.
        if akuma_exec::mmu::user_flags::is_none(flags) {
            return false;
        }
        if matches!(source, akuma_exec::process::LazySource::Zero) {
            let map_flags = if flags != 0 { flags } else { akuma_exec::mmu::user_flags::RW };
            // The alloc stays OUTSIDE the `as_lock` hold below: under PMM pressure
            // `alloc_page_zeroed_user` calls `reclaim_clean_file_pages`, which takes
            // `as_lock` once per swept page — allocating under the hold would
            // self-deadlock on a non-reentrant `Spinlock`.
            if let Some(page_frame) = crate::pmm::alloc_page_zeroed_user() {
                // PTE install + frame bookkeeping under `as_lock` (Phase 7f
                // pre-flight; same fold Phase 7e applied to the signal-path PTE
                // sites). `as_owner` is the L0-owning thread-group leader, so this is
                // the lock that actually guards these tables, and it is also the
                // process the frames are tracked against. The two steps must be
                // atomic against a peer's `try_evict_ro_page` on this page: it clears
                // a live RO PTE and declines to free an untracked frame, so the
                // mapped-but-untracked instant is a re-fault leak.
                let owner = akuma_exec::process::lookup_process_shared(as_owner);
                let install = || {
                    let (table_frames, installed) = unsafe {
                        akuma_exec::mmu::map_user_page(page_va, page_frame.addr, map_flags)
                    };
                    if let Some(owner) = owner {
                        if installed {
                            owner.address_space.track_user_frame(page_frame);
                        }
                        for tf in &table_frames {
                            owner.address_space.track_page_table_frame(*tf);
                        }
                    }
                    (table_frames, installed)
                };
                let (table_frames, installed) = match owner {
                    Some(owner) => owner.with_as_locked(install),
                    None => install(),
                };
                // Frees after the hold is released; ownership rules unchanged — the
                // data frame goes back to the PMM iff nothing mapped it (lost CAS
                // race) or there is no owner to track it against.
                if !installed || owner.is_none() {
                    crate::pmm::free_page(page_frame);
                }
                if owner.is_none() {
                    for tf in table_frames { crate::pmm::free_page(tf); }
                }
                return true;
            }
        }
    }
    false
}

/// Fixed VA where the rt_sigreturn trampoline is mapped in every user process.
/// Go on arm64 does not set SA_RESTORER and relies on the kernel/vDSO to provide
/// the return stub.  We map this page lazily on first signal delivery.
const SIGRETURN_TRAMPOLINE_ADDR: usize = 0x2000;

/// Ensure the rt_sigreturn trampoline page is mapped at SIGRETURN_TRAMPOLINE_ADDR
/// in the current process.  Returns Some(SIGRETURN_TRAMPOLINE_ADDR) on success.
///
/// AArch64 trampoline:
///   movz x8, #139   ; SYS_rt_sigreturn
///   svc  #0
fn ensure_sigreturn_trampoline(pid: u32) -> Option<usize> {
    let as_owner = akuma_exec::process::address_space_owner_pid_for_fault().unwrap_or(pid);
    // movz x8, #139 = 0xD2801168 (LE: 68 11 80 D2)
    // svc  #0       = 0xD4000001 (LE: 01 00 00 D4)
    const TRAMPOLINE: [u8; 8] = [0x68, 0x11, 0x80, 0xD2, 0x01, 0x00, 0x00, 0xD4];

    if akuma_exec::mmu::is_current_user_page_mapped(SIGRETURN_TRAMPOLINE_ADDR) {
        return Some(SIGRETURN_TRAMPOLINE_ADDR);
    }

    let frame = crate::pmm::alloc_page_zeroed()?;
    unsafe {
        let ptr = akuma_exec::mmu::phys_to_virt(frame.addr).cast::<u8>();
        core::ptr::copy_nonoverlapping(TRAMPOLINE.as_ptr(), ptr, TRAMPOLINE.len());
    }

    let (table_frames, installed) = unsafe {
        akuma_exec::mmu::map_user_page(SIGRETURN_TRAMPOLINE_ADDR, frame.addr, akuma_exec::mmu::user_flags::RX)
    };

    if let Some(owner) = akuma_exec::process::lookup_process_shared(as_owner) {
        if installed {
            owner.address_space.track_user_frame(frame);
        } else {
            crate::pmm::free_page(frame);
        }
        for tf in table_frames {
            owner.address_space.track_page_table_frame(tf);
        }
    } else {
        crate::pmm::free_page(frame);
        for tf in table_frames { crate::pmm::free_page(tf); }
        return None;
    }

    Some(SIGRETURN_TRAMPOLINE_ADDR)
}

/// Syscalls Linux never restarts, regardless of `SA_RESTART`. Either the caller
/// must re-evaluate a predicate the handler just changed (`rt_sigsuspend`/
/// `pause`), or it was given an explicit timeout that a silent restart would
/// extend past (`ppoll`/`pselect6`/`epoll_pwait`). AArch64 generic syscall
/// numbers.
///
/// Restarting `rt_sigsuspend` after SIGCHLD delivery is precisely fatal for
/// busybox ash's `wait`: the handler set `got_sigchld`, `rt_sigreturn` re-enters
/// the `SVC`, the now-consumed pending bit is gone, and the kernel cannot see
/// the userspace `got_sigchld` flag — so it suspends forever, reproducing the
/// exact hang SIGCHLD delivery was meant to fix.
pub fn syscall_is_non_restartable(nr: u64) -> bool {
    matches!(nr,
        4    /* io_getevents */
        | 22 /* epoll_pwait */
        | 72 /* pselect6 */
        | 73 /* ppoll */
        | 133 /* rt_sigsuspend */)
}

/// Try to deliver a signal to a userspace handler by setting up an
/// rt_sigframe on the user stack and redirecting ELR to the handler.
/// Returns true if delivery succeeded (caller should return signal number as x0).
fn try_deliver_signal(frame: *mut UserTrapFrame, signal: u32, fault_addr: u64, is_fault: bool, entry_esr: u64) -> bool {
    // Record the delivery for the blocking-syscall EINTR path BEFORE anything can
    // return early. `take_pending_signal` has already cleared the pending bit at
    // every caller, so this record is the only remaining evidence that this thread
    // was signalled during its current syscall — and a blocking wait loop needs
    // that evidence to report EINTR. Recording here rather than at the seven call
    // sites keeps the two in step: a new delivery path gets it for free.
    // docs/archive/PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md
    akuma_exec::threading::note_delivered_signal(
        akuma_exec::threading::current_thread_id(),
        signal,
    );
    let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let proc = match akuma_exec::process::lookup_process_shared(pid) {
        Some(p) => p,
        None => return false,
    };

    let idx = (signal as usize).wrapping_sub(1);
    if idx >= akuma_exec::process::MAX_SIGNALS {
        return false;
    }

    let action = {
        let actions = proc.signal_actions.actions.lock();
        actions[idx]
    };

    let handler_addr = match action.handler {
        akuma_exec::process::SignalHandler::UserFn(addr) => addr,
        _ => return false,
    };

    // SA_RESTART (ARM64 nr=0x10000000)
    // If the signal was delivered during a syscall, and SA_RESTART is set,
    // we want the syscall to be re-executed after the handler returns.
    // In Linux, this is often done via ERESTARTSYS. Here we do it manually
    // by backing up ELR to the SVC instruction.
    //
    // entry_esr is the ESR snapshotted by the vector asm at exception entry
    // (while PSTATE.I was still masked), before any preemption could overwrite
    // ESR_EL1 on the live register.
    const SA_RESTART: u64 = 0x10000000;
    if action.flags & SA_RESTART != 0 {
        // Only if we were in a syscall (EC_SVC_LOWER)
        if (entry_esr >> 26) == 0x15 { // EC_SVC_LOWER
            // Only restart the syscall if it was actually interrupted.
            // SA_RESTART must NOT apply to successful syscalls — backing up ELR
            // for a completed FUTEX_WAKE (ret=1) causes it to re-execute with
            // x0=1 (the return value), producing EINVAL (uaddr=1 is unaligned).
            let ret_val = unsafe { (*frame).x0 as i64 };
            // And never restart the non-restartable set (sigsuspend/ppoll/…):
            // their contract is to return EINTR so the caller re-checks a
            // predicate the handler changed. frame.x8 still holds the syscall
            // number at this point.
            let syscall_nr = unsafe { (*frame).x8 };
            if (ret_val == -4 /* EINTR */ || ret_val == -512 /* ERESTARTSYS */)
                && !syscall_is_non_restartable(syscall_nr)
            {
                unsafe { (*frame).elr_el1 -= 4; }
            }
        }
    }

    // When the process didn't register a restorer (Go on arm64 relies on the
    // vDSO instead of SA_RESTORER), lazily map our kernel-provided trampoline.
    let restorer = if action.restorer != 0 {
        action.restorer
    } else if let Some(addr) = ensure_sigreturn_trampoline(pid) { addr } else {
        crate::tprint!(64, "[signal] failed to map sigreturn trampoline for pid={}\n", pid);
        return false;
    };
    let frame_ref = unsafe { &*frame };
    let user_sp = frame_ref.sp_el0 as usize;

    // Detect re-entrant signal: if sp is already on the sigaltstack, we are
    // inside a signal handler that itself faulted.  Re-delivering would cause
    // an infinite loop (the handler keeps faulting on the same address).
    // Terminate instead, which matches Linux's default behaviour when a fatal
    // signal fires with SA_NODEFER not set (the signal is masked during handler
    // execution so a second delivery goes to the default action = termination).
    // Use per-thread sigaltstack (indexed by kernel thread slot) so that
    // CLONE_VM threads each maintain their own independent gsignal stack.
    let thread_slot = akuma_exec::threading::current_thread_id();
    let (alt_sp, alt_size, _alt_flags) = akuma_exec::threading::get_sigaltstack(thread_slot);

    // `fault_pc` is the saved ELR at exception entry — i.e. the user PC where the
    // fault/interrupt occurred — *not* the handler we will install at handler_addr.
    // Misreading this as “handler PC” suggests ELR corruption; it is not.
    crate::tprint!(256,
        "[signal] deliver sig={} slot={} handler={:#x} fault_pc={:#x} user_sp={:#x} alt_sp={:#x} alt_size={:#x} sa_flags={:#x}\n",
        signal, thread_slot, handler_addr, frame_ref.elr_el1, user_sp, alt_sp, alt_size, action.flags);

    if crate::config::DEBUG_PATTERN2_TRAP_TRACE && syscall_stub_elr_in_diag_window(frame_ref.elr_el1) {
        crate::tprint!(
            192,
            "[pattern2-stub] deliver sig={} pid={} slot={} fault_pc={:#x} x8={:#x} ({}) sp={:#x}\n",
            signal,
            pid,
            thread_slot,
            frame_ref.elr_el1,
            frame_ref.x8,
            syscall_nr_pattern2_hint(frame_ref.x8),
            user_sp,
        );
    }

    // If the handler requires SA_ONSTACK but no sigaltstack is configured for
    // this thread yet (e.g. SIGURG arrives before Go M calls sigaltstack during
    // mstart), delivering on the goroutine stack would corrupt goroutine data
    // (asyncPreempt2 may grow the goroutine stack into goroutine variables).
    // Re-pend the signal so it is retried at the next syscall boundary, by
    // which time mstart will have called sigaltstack.
    if (action.flags & SA_ONSTACK) != 0 && alt_sp == 0 {
        crate::tprint!(128,
            "[signal] sig {} needs sigaltstack but slot {} has none — re-pending\n",
            signal, thread_slot);
        akuma_exec::threading::pend_signal_for_thread(thread_slot, signal);
        return false;
    }

    if alt_sp != 0 {
        let alt_lo = alt_sp as usize;
        let alt_hi = alt_lo + alt_size as usize;
        if user_sp >= alt_lo && user_sp < alt_hi {
            if !is_fault {
                // Non-fault signal (e.g. SIGURG async preemption) arrived while Go's
                // signal handler is running on sigaltstack.  Re-pend it for delivery
                // after sigreturn instead of silently dropping it.  Mirrors the
                // existing re-pend path for when sigaltstack isn't configured yet
                // (lines above).  The caller will NOT kill the process.
                crate::tprint!(128,
                    "[signal] sig {} re-entrant on sigaltstack (sp={:#x} in [{:#x},{:#x})) \
                     — re-pending\n",
                    signal, user_sp, alt_lo, alt_hi);
                akuma_exec::threading::pend_signal_for_thread(thread_slot, signal);
            } else {
                // Fatal signal (e.g. re-entrant SIGSEGV) while inside a signal handler —
                // genuine unrecoverable crash.  The data-abort caller falls through to
                // return_to_kernel(-11).
                crate::tprint!(128,
                    "[signal] sig {} re-entrant FAULT at {:#x} (sp={:#x} on sigaltstack \
                     [{:#x},{:#x})) — killing process\n",
                    signal, fault_addr, user_sp, alt_lo, alt_hi);
            }
            return false;
        }
    }

    // If SA_ONSTACK is set and a sigaltstack is configured, deliver on the
    // alternate signal stack rather than the current goroutine/thread stack.
    // Go (and other runtimes) require this to detect which stack a signal
    // arrived on; without it, Go panics with "handler not on signal stack".
    let stack_top = if (action.flags & SA_ONSTACK) != 0
        && alt_sp != 0
        && alt_size >= SIGFRAME_SIZE as u64
    {
        (alt_sp + alt_size) as usize
    } else {
        user_sp
    };

    let new_sp = (stack_top - SIGFRAME_SIZE) & !0xF;

    crate::tprint!(256,
        "[signal] frame: stack_top={:#x} new_sp={:#x} on_altstack={}\n",
        stack_top, new_sp, stack_top != user_sp);

    // Ensure stack pages are mapped (signal frame may span 2 pages).
    // Demand-page lazy anonymous stack pages if not yet mapped.
    let first_page = new_sp & !0xFFF;
    let last_page = (new_sp + SIGFRAME_SIZE - 1) & !0xFFF;
    if !ensure_user_page_mapped(pid, first_page) {
        crate::tprint!(128, "[signal] sig {} frame page {:#x} not mappable\n", signal, first_page);
        return false;
    }
    // Pre-resolve CoW: ensure the page is writable before the kernel writes the
    // signal frame from EL1. Without this, a CoW-demoted altstack page (mapped RO)
    // causes EC=0x25 (EL1 data abort) when write_bytes runs below.
    ensure_cow_page_writable(pid, first_page);
    if last_page != first_page && !ensure_user_page_mapped(pid, last_page) {
        crate::tprint!(128, "[signal] sig {} frame page {:#x} not mappable\n", signal, last_page);
        return false;
    }
    if last_page != first_page { ensure_cow_page_writable(pid, last_page); }

    // Build the whole frame on the kernel stack, then copy it out once. The
    // `#[repr(C)]` layout and every offset in it are checked at compile time in
    // `akuma_exec::threading::sigframe`; what used to be ~130 `write(base.add(N))`
    // calls straight into user memory is now field assignment plus one copy.
    let mut sf = sigframe::RtSigFrame::zeroed();

    // siginfo_t
    sf.info.si_signo = signal as i32;
    sf.info.si_errno = 0;
    const CLD_EXITED: i32 = 1;
    const CLD_KILLED: i32 = 2;
    if signal == 17 /* SIGCHLD */ {
        // Fill the `_sigchld` union arm. The payload was stashed by
        // `raise_sigchld_for_parent` in the per-thread `LAST_SIGCHLD` side-channel;
        // peek (not take) so a signal that re-pends (SA_ONSTACK before sigaltstack)
        // still finds it.
        let (child_pid, raw_code) =
            akuma_exec::threading::peek_last_sigchld(thread_slot).unwrap_or((0, 0));
        // Negative raw_code ⇒ killed by signal (-raw_code); else clean exit.
        let (si_code, si_status) = if raw_code < 0 {
            (CLD_KILLED, -raw_code)
        } else {
            (CLD_EXITED, raw_code)
        };
        sf.info.si_code = si_code;
        // si_uid = 0: every process here runs as root.
        sf.info.fields.chld = sigframe::SigchldFields::new(child_pid, 0, si_status);
    } else {
        sf.info.si_code = i32::from(is_fault); // SEGV_MAPERR=1, SI_USER=0
        sf.info.fields.fault = sigframe::SigfaultFields { si_addr: fault_addr };
    }

    // ucontext.uc_stack (stack_t) — Go runtime reads this to determine
    // whether the signal arrived on the sigaltstack.  All-zero confuses
    // Go's panic recovery and can produce corrupted SP/PSTATE on sigreturn.
    let on_altstack = stack_top != user_sp;
    sf.uc.uc_stack.ss_sp = alt_sp;
    sf.uc.uc_stack.ss_flags = if on_altstack { sigframe::SS_ONSTACK } else { 0 };
    sf.uc.uc_stack.ss_size = alt_size;
    // uc_sigmask — the mask `rt_sigreturn` will restore. Normally the current
    // per-thread mask (NOT proc.signal_mask, which is shared across CLONE_THREAD
    // siblings via the owner PID) — see docs/SIGNAL_DELIVERY_FORKTEST_EVIDENCE.md §D.
    // If rt_sigsuspend armed a restore-mask, save THAT instead so sigreturn
    // restores the pre-suspend mask (POSIX sigsuspend semantics, §7k.5).
    //
    // This *takes* the armed mask, and the copy below can now fail where the old
    // per-field writes could not — so a failed copy consumes it. That path declines
    // delivery, which ends in the default action (termination for every signal that
    // reaches it), so there is no thread left to restore the mask into.
    sf.uc.uc_sigmask = akuma_exec::threading::take_restore_sigmask()
        .unwrap_or_else(akuma_exec::threading::thread_signal_mask);

    // mcontext_t (sigcontext): the interrupted user context, x0–x30 + sp/pc/pstate.
    sf.save_regs(frame_ref, fault_addr);

    // FPSIMD extension record. The kernel never uses FP, so what `sync_el0_handler`
    // saved in this trap frame's NEON area is still the user's.
    //
    // SAFETY: every `try_deliver_signal` caller is in the EL0 **sync** handler, whose
    // 832-byte frame is the layout `sync_frame_neon` names. (The EL0 *IRQ* frame is
    // also 832 bytes and puts its NEON block 16 bytes earlier.)
    let neon = unsafe { &*sigframe::sync_frame_neon(frame) };
    sf.fpsimd.magic = FPSIMD_MAGIC;
    sf.fpsimd.size = 528;
    // FPSR/FPCR are 32-bit registers stored in 64-bit slots on the kernel stack.
    sf.fpsimd.fpsr = neon.fpsr as u32;
    sf.fpsimd.fpcr = neon.fpcr as u32;
    sf.fpsimd.vregs = neon.vregs;
    // The `_aarch64_ctx{0,0}` terminator after the record is already zero.

    // One copy out, in place of ~130 individual writes into user memory.
    //
    // `Prefault::No` because this runs on a fault-handling stack, where
    // `prefault_user_range`'s frame allocation and `as_lock` acquisition are not
    // allowed — the `ensure_user_page_mapped` + `ensure_cow_page_writable` pre-flight
    // above is the fault-safe form of the same job, and it has already run for both
    // pages this frame can span.
    if akuma_exec::mmu::user_access::write_user_val_with(
        new_sp as u64,
        &sf,
        akuma_exec::mmu::user_access::Prefault::No,
    )
    .is_err()
    {
        // The pre-flight just mapped and CoW-resolved both pages, so this fires only
        // when `new_sp` is not EL0-accessible at all — a stack pointer inside an
        // EL1-only mapping, which the old open-coded writes followed into kernel
        // memory (`USER_COPY_FOLD.md` §7). Declining delivery here means the caller
        // applies the default action instead.
        crate::tprint!(128, "[signal] sig {} frame copy to {:#x} failed\n", signal, new_sp);
        return false;
    }

    unsafe {
        // Redirect execution to the signal handler
        (*frame).elr_el1 = handler_addr as u64;
        (*frame).sp_el0 = new_sp as u64;
        (*frame).x30 = restorer as u64;

        // Demand-paged or RW-only mappings can leave the handler/restorer pages
        // non-executable; without RX, ERET to the handler faults immediately.
        // (If fault_pc is in the kernel-RAM VA range below KERNEL_TEXT_END, that is a
        // separate bug: user tried to *execute* identity-mapped RAM — usually UXN.)
        let handler_va = handler_addr & !0xFFF;
        let restorer_va = restorer & !0xFFF;
        // PTE permission edits under `as_lock` (`with_address_space`); the
        // icache invalidates ride in the same short hold to keep the old order.
        proc.with_address_space(|aspace| {
            let _ = aspace.update_page_flags(handler_va, akuma_exec::mmu::user_flags::RX);
            let _ = aspace.update_page_flags(restorer_va, akuma_exec::mmu::user_flags::RX);
            aspace.invalidate_icache_for_page_va(handler_va);
            aspace.invalidate_icache_for_page_va(restorer_va);
        });

        if action.flags & SA_SIGINFO != 0 {
            (*frame).x1 = (new_sp + SIGFRAME_SIGINFO) as u64;
            (*frame).x2 = (new_sp + SIGFRAME_UCONTEXT) as u64;
        }
    }

    // Block the delivered signal and the sa_mask signals during handler execution.
    // Per-thread mask (see uc_sigmask note above): blocking on the shared
    // proc.signal_mask would (un)block the signal for sibling threads too.
    if action.flags & SA_NODEFER == 0 && (1..=64).contains(&signal)
        && signal != 9 && signal != 19 { // SIGKILL/SIGSTOP cannot be masked
            akuma_exec::threading::or_thread_signal_mask(1u64 << (signal - 1));
        }
    // Also apply the additional mask from sigaction(2): sa_mask is the set of signals
    // blocked while this handler runs.  SIGKILL (bit 8) and SIGSTOP (bit 18) are immune.
    akuma_exec::threading::or_thread_signal_mask(action.mask & !((1u64 << 8) | (1u64 << 18)));

    crate::tprint!(128, "[signal] Delivering sig {} to handler {:#x} (restorer={:#x})\n",
        signal, handler_addr, restorer);
    true
}

/// Apply the *default* action for a pended signal that `try_deliver_signal`
/// declined, terminating the thread group when that action is termination.
///
/// `try_deliver_signal` only ever installs a **user** handler, so it returns
/// false for two unrelated reasons: the disposition is SIG_DFL/SIG_IGN (nothing
/// to jump to), or a `UserFn` delivery was deliberately re-pended (no
/// sigaltstack yet, re-entrant on the altstack). Re-reading the action here
/// separates them — only SIG_DFL is a disposition question, and the re-pend
/// paths are unreachable without a `UserFn` handler, so they are never mistaken
/// for "kill me".
///
/// Callers previously just returned normally on false, which silently **dropped**
/// every fatal SIG_DFL signal that arrived via the pending queue. That is why
/// `abort()` never worked from a spawned thread: musl blocks SIGABRT, `tkill`s
/// itself — and `sys_tkill` pends rather than acts when the signal is blocked —
/// then unblocks it. Delivery at that `rt_sigprocmask` return found SIG_DFL,
/// returned false, and discarded the signal, so musl fell through to its
/// `a_crash()` (`strb wzr, [x0]`) and the process died reporting *SIGSEGV at
/// FAR=0* instead of SIGABRT. Repro: `userspace/forktest/c_stress/abortsig.c`.
///
/// Fatality uses `signal_is_fatal_default`, which is deliberately conservative
/// (no SIGUSR1/2, no RT signals — see its doc comment); this path must not turn
/// the self-host build's SIGUSR1 storm into a kill.
///
/// Diverges when the signal is fatal; returns normally otherwise.
fn apply_default_signal_action(signal: u32) {
    if crate::config::TRACE_TKILL {
        crate::safe_print!(96, "[signal] default-action check sig={} slot={}\n",
            signal, akuma_exec::threading::current_thread_id());
    }
    let Some(pid) = akuma_exec::process::read_current_pid() else { return };
    let Some(proc) = akuma_exec::process::lookup_process_shared(pid) else { return };

    let idx = (signal as usize).wrapping_sub(1);
    if idx >= akuma_exec::process::MAX_SIGNALS {
        return;
    }
    let handler = {
        let actions = proc.signal_actions.actions.lock();
        actions[idx].handler
    };
    if !matches!(handler, akuma_exec::process::SignalHandler::Default) {
        return;
    }
    if !crate::syscall::signal::signal_is_fatal_default(signal) {
        return;
    }

    crate::safe_print!(128,
        "[signal] Process {} ({}) terminated by signal {} (default action)\n",
        proc.pid, proc.name, signal);
    crate::syscall::proc::sys_exit_group_pub(-(signal as i32)) // never returns
}

/// Terminal path for a fatal EL0 fault whose signal reached its *default* action
/// (no `UserFn` handler, or a handler that reset itself to `SIG_DFL` and returned
/// so the faulting instruction re-executed — what Rust's stack-overflow handler
/// does for any address outside the guard page).
///
/// A fatal default-action signal kills the whole **thread group**, not just the
/// faulting thread (Linux routes it through `do_group_exit`). `sys_exit_group` is
/// the only exit path in this kernel whose ordering makes that hold:
///
/// ```text
/// kill_thread_group  ->  fds.close_all  ->  notify parent  ->  self-terminate
/// ```
///
/// **Why the ordering is the whole fix.** These sites used to call
/// `notify_child_channel_exited_pub` first and then fall into `return_to_kernel`,
/// whose own `kill_thread_group` call sits *after* that notify. The notify wakes
/// the parent's `wait4`, and on a peer core the parent can reap us
/// (`unregister_process`) before we get there. `return_to_kernel` then finds
/// `current_process_shared() == None`, takes its `pid = None` branch, and skips
/// its **entire** cleanup block — no `cleanup_process_fds`, no
/// `kill_child_processes`, no `kill_thread_group`, no `unregister_process`. Every
/// sibling `CLONE_VM` thread is orphaned: never terminated, never reaped, parked
/// in `FUTEX_WAIT` with a live `Process` row and a pinned address space for the
/// rest of the boot.
///
/// Measured, `/tmp/akuma-debug/serial.log` (2026-08-12, `SMP=4` devbox-smoltcp):
/// cargo pid 151 crashed at T219.58, its `[TERM] tid=14 pid=Some(151) by_tid=18`
/// came from *bash's* reap, no `[KTG] my_pid=151` line was ever printed, and tids
/// 12/13/22/29 of tgid 151 were still burning CPU in the futex table at T510 —
/// five minutes later. pid 1280 reproduced it identically at T520.78. Writeup:
/// `docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md` §13.5.
///
/// `sys_exit_group_pub` falls through to `return_to_kernel` when there is no
/// current process (kernel helper thread), which is the old behaviour for that
/// case, and it never returns for a real user thread.
///
/// The `is_shared()` gate this replaced only fired for `CLONE_VM` threads, so the
/// common case — a multi-threaded process crashing on its **main** thread, which
/// owns a non-shared address space — was precisely the one that leaked.
fn fatal_signal_group_exit(code: i32) -> ! {
    crate::syscall::proc::sys_exit_group_pub(code)
}

/// Restore saved context from a signal frame on the user stack (rt_sigreturn).
/// Returns the saved x0 value, or None if the frame is invalid.
///
/// **Forktest Pattern 2:** full GPR restore including **`x8`** — if anything in the
/// sigframe path is wrong, the next SVC can see bogus syscall numbers or arguments
/// (see `docs/GO_FORKTEST_DEBUG.md`, Agent handoff). Pending **`SIGURG`** delivery
/// immediately after sigreturn is handled in the **`syscall_num == 139`** branch in
/// `rust_sync_el0_handler`.
fn do_rt_sigreturn(frame: *mut UserTrapFrame) -> Option<u64> {
    // SAFETY: the EL0 sync handler passes its own trap frame, live for this call, and
    // nothing else holds a reference to it. One borrow for the whole function so the
    // field accesses below need no further `unsafe`.
    let f = unsafe { &mut *frame };
    let sigframe_sp = f.sp_el0;

    // Read the frame in one copy instead of ~40 `read(sigframe_sp + N)`s.
    //
    // `Prefault::No` keeps the old behaviour: this used to require both frame pages to
    // be *present* and gave up otherwise, and a sigframe SP pointing at an unfaulted
    // lazy page is a corrupt frame, not something to demand-page. What the validated
    // read adds over the old presence test is the AP check — a frame SP inside an
    // EL1-only mapping is now rejected instead of read (`USER_COPY_FOLD.md` §7).
    let mut sf = sigframe::RtSigFrame::zeroed();
    if akuma_exec::mmu::user_access::read_user_into_with(
        &mut sf,
        sigframe_sp,
        akuma_exec::mmu::user_access::Prefault::No,
    )
    .is_err()
    {
        return None;
    }

    // Full GPR restore including x8 — see the doc comment: a wrong register here shows
    // up as the *next* SVC seeing a bogus syscall number.
    let restored_spsr = sf.restore_regs(f);

    // Validate SPSR: must be EL0t (M[4:0] = 0).  Go's signal handler can
    // corrupt the signal frame (the delivery path notes "Go's panic recovery
    // can produce corrupted SP/PSTATE on sigreturn").  If M[4]=1 (AArch32
    // mode) or any other invalid mode bits are set, ERET would crash the
    // kernel.  Force clean EL0t instead.
    if restored_spsr & 0x1F != 0 {
        crate::tprint!(128,
            "[sigreturn] WARNING: corrupted SPSR={:#x} (mode bits={:#x}), forcing EL0t\n",
            restored_spsr, restored_spsr & 0x1F);
        // Clear only the M[4:0] mode bits; preserve NZCV, DAIF, and other flags
        // so that Go's signal-handler modifications to pstate (e.g. NZCV) survive.
        f.spsr_el1 = restored_spsr & !0x1F;
    } else {
        f.spsr_el1 = restored_spsr;
    }

    crate::tprint!(256,
        "[sigreturn] restoring: sp={:#x} pc={:#x} pstate={:#x} sigframe_sp={:#x}\n",
        f.sp_el0, f.elr_el1, f.spsr_el1, sigframe_sp);

    if crate::config::DEBUG_PATTERN2_TRAP_TRACE && syscall_stub_elr_in_diag_window(f.elr_el1) {
        let rp = akuma_exec::process::read_current_pid().unwrap_or(0);
        let slot = akuma_exec::threading::current_thread_id();
        crate::tprint!(
            224,
            "[pattern2-sigreturn] pid={} slot={} restored_pc={:#x} x8={:#x} ({}) sigframe_sp={:#x}\n",
            rp,
            slot,
            f.elr_el1,
            f.x8,
            syscall_nr_pattern2_hint(f.x8),
            sigframe_sp,
        );
    }

    // Restore signal mask from uc_sigmask into the PER-THREAD mask
    // (set_thread_signal_mask drops SIGKILL/SIGSTOP). Restoring into the shared
    // proc.signal_mask is the bug that let one sibling's sigreturn clobber another
    // thread's block — docs/SIGNAL_DELIVERY_FORKTEST_EVIDENCE.md §D.
    akuma_exec::threading::set_thread_signal_mask(sf.uc.uc_sigmask);

    // Restore FPSIMD state from the signal frame into the trap frame's NEON save area;
    // sync_el0_handler restores the registers from there after this returns.
    if sf.fpsimd.magic == FPSIMD_MAGIC {
        // SAFETY: `do_rt_sigreturn` is called only from the EL0 sync handler, so `frame`
        // is the 832-byte sync frame whose layout `sync_frame_neon` names. The `&mut f`
        // borrow above covers only the GPR block, which this area sits past.
        let neon = unsafe { &mut *sigframe::sync_frame_neon(frame) };
        neon.fpsr = u64::from(sf.fpsimd.fpsr);
        neon.fpcr = u64::from(sf.fpsimd.fpcr);
        neon.vregs = sf.fpsimd.vregs;
    }

    Some(sf.uc.uc_mcontext.regs[0])
}

/// Install exception vector table
pub fn init() {
    // Initialize exception stack before enabling exceptions
    init_exception_stack();

    unsafe {
        let vbar = &raw const exception_vector_table as u64;

        // Set VBAR_EL1 (Vector Base Address Register)
        core::arch::asm!(
            "msr vbar_el1, {vbar}",
            "isb",
            vbar = in(reg) vbar
        );

    }

    // Enable IRQs by clearing DAIF.I, now that VBAR_EL1 is installed and
    // synchronized above — the ordering that makes taking an interrupt safe.
    akuma_primitives::irq::unmask_irqs();
}

/// Default exception handler - logs unexpected exceptions
/// CRITICAL: Must NOT return if ELR/SPSR are invalid, or ERET will crash!
#[unsafe(no_mangle)]
extern "C" fn rust_default_exception_handler() {
    note_exception_entry();
    note_exc_class(3);
    let esr: u64;
    let elr: u64;
    let spsr: u64;
    let ttbr0: u64;
    let sp: u64;
    unsafe {
        core::arch::asm!("mrs {}, esr_el1", out(reg) esr);
        core::arch::asm!("mrs {}, elr_el1", out(reg) elr);
        core::arch::asm!("mrs {}, spsr_el1", out(reg) spsr);
        core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr0);
        core::arch::asm!("mov {}, sp", out(reg) sp);
    }
    let ec = (esr >> 26) & 0x3F;
    let tid = akuma_exec::threading::current_thread_id();
    
    // Use stack-only print to avoid heap allocation in exception context
    crate::safe_print!(128, "[Exception] Default handler: EC={:#x}, ELR={:#x}, SPSR={:#x}\n",
        ec, elr, spsr);
    crate::safe_print!(96, "  Thread={}, TTBR0={:#x}, SP={:#x}\n", tid, ttbr0, sp);
    
    // Check for dangerous ERET conditions
    let target_el = spsr & 0xF;
    if target_el == 0 {
        crate::safe_print!(128, "  WARNING: SPSR indicates EL0 - ERET would go to user mode!\n");
    }
    if elr == 0 {
        crate::safe_print!(128, "  WARNING: ELR=0 - ERET would jump to address 0!\n");
    }
    if elr < 0x4000_0000 && target_el != 0 {
        crate::safe_print!(96, "  WARNING: ELR={:#x} looks like user address but SPSR is EL1!\n", elr);
    }
    
    // If ERET would be dangerous, halt instead of returning
    if elr == 0 || (target_el == 0 && elr < 0x4000_0000) {
        crate::safe_print!(64, "  HALTING to prevent invalid ERET\n");
        loop {
            unsafe { core::arch::asm!("wfe"); }
        }
    }
}

/// UNIFIED IRQ handler for stack-based context switching
/// 
/// Used by both irq_el0_handler (user mode IRQs) and irq_handler (kernel mode IRQs).
/// 
/// Takes current SP, returns new SP if context switch needed (or 0 if no switch).
/// The assembly does the actual SP switch AFTER this returns.
/// Byte offset of the saved `SPSR_EL1` within the 832-byte IRQ trap frame (the frame
/// `sp` points at when the vector calls `rust_irq_handler_with_sp`). Derived from the
/// save order in `irq_el0_handler`/`irq_handler`: x30(0), x28/29..x0/1 (240), then
/// ELR(240)/SPSR(248). Synthetic frames written by `setup_fake_irq_frame` place SPSR
/// at the same slot (`frame.add(31)` = byte 248). The BKL reads it to learn the EL the
/// `eret` will target. If the frame layout ever changes, update this constant.
#[cfg(kernel_smp_shared)]
const IRQ_FRAME_SPSR_OFFSET: usize = 248;

/// Tripwire for the SMP=4 mixed-EL corruption (BKL_RUSTC_SCALING_BASELINE.md §5.1):
/// inspect the IRQ frame the asm epilogue is about to restore (ELR at +240, SPSR at
/// +248 — see `IRQ_FRAME_SPSR_OFFSET`). An EL0-target frame whose ELR sits in kernel
/// text would eret userspace into kernel text. Covers BOTH epilogue outcomes (switch
/// and no-switch); complements `[SGI-S POISON]`, which only sees the switch branch.
#[inline]
#[allow(clippy::verbose_bit_mask)]
fn irq_eret_poison_check(final_sp: u64, switched: bool) {
    // SAFETY: `final_sp` is the live IRQ trap frame the asm restores next.
    let elr = unsafe { core::ptr::read_volatile((final_sp as usize + 240) as *const u64) };
    let spsr = unsafe { core::ptr::read_volatile((final_sp as usize + 248) as *const u64) };
    let kernel_text = akuma_exec::mmu::is_kernel_text(elr as usize);
    // EL0-target frames must not eret into kernel text; EL1-target frames must
    // eret INTO kernel text (an EL1 ELR like 0x8 is the boot-storm crash shape).
    let poison = if (spsr & 0xF) == 0 { kernel_text } else { !kernel_text };
    if poison {
        crate::safe_print!(160,
            "[IRQ POISON] eret elr={:#x} spsr={:#x} switched={} tid={} core={}\n",
            elr, spsr, u64::from(switched),
            akuma_exec::threading::current_thread_id(),
            akuma_exec::bkl::current_core_id());
    }
}

/// IRQ / SGI entry from the vector asm. Under real shared-kernel SMP this runs the
/// scheduler and device handlers holding the Big Kernel Lock, then reconciles the BKL
/// to the EL the pending `eret` will enter (release when returning to EL0, keep for
/// EL1) using the SPSR of the frame we're about to restore — which, after a context
/// switch, is the *incoming* thread's frame. No-op unless `cfg(kernel_smp_shared)`.
#[unsafe(no_mangle)]
extern "C" fn rust_irq_handler_with_sp(current_sp: u64) -> u64 {
    note_exception_entry();
    note_exc_class(2);
    // Under shared SMP, read the interrupted frame's SPSR up front: SPSR.M[3:0]==0 means
    // we preempted EL0 (userspace), where this core holds NO BKL (the invariant is "held
    // iff in EL1"). That is exactly the case where the scheduler SGI can run BKL-FREE
    // (M5c) — the switch is made atomic by POOL alone, so peer cores' timer ticks no
    // longer serialize on the BKL. A scheduler SGI that preempted EL1 (a syscall/fault
    // holding the BKL) must keep it — releasing would expose the interrupted excursion's
    // shared state to peers.
    #[cfg(kernel_smp_shared)]
    #[allow(clippy::verbose_bit_mask)]
    let interrupted_el0 = {
        // SAFETY: `current_sp` is the live interrupted IRQ trap frame; SPSR at fixed off.
        let cur_spsr = unsafe {
            core::ptr::read_volatile((current_sp as usize + IRQ_FRAME_SPSR_OFFSET) as *const u64)
        };
        // SPSR.M[3:0] == 0 ⇒ interrupted context was EL0t (userspace).
        (cur_spsr & 0xf) == 0
    };
    // If this IRQ interrupted EL0, userspace was running on this core — count it (captures
    // pure compute loops that only get timer-preempted, not just syscalls).
    #[cfg(kernel_smp_shared)]
    if interrupted_el0 {
        crate::smp_shared::record_el0_trap();
    }

    // Acknowledge the IRQ once, up front (the GIC IAR read needs no BKL).
    let irq_opt = crate::gic::acknowledge_irq();
    match irq_opt {
        Some(intid) => note_irq_intid(intid),
        // IAR said "spurious" (1023): the vector fired but the interrupt was
        // already consumed (peer core, or a de-asserted level source). These
        // entries are invisible to the per-INTID counters yet still count in
        // EXCEPTION_ENTRIES — exactly the gap the ~140K/s/core unattributed
        // exception storm hid in (CROSS_CORE_THREAD_COLLAPSE.md §3).
        None => {
            SPURIOUS_IRQS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
    }

    // BKL-free fast path (M5c): a scheduler SGI that preempted EL0. This core held no BKL,
    // and `sgi_scheduler_handler_with_sp` makes the switch atomic on POOL. Reconcile at
    // the end (re)acquires the BKL only if the thread we resume into is EL1.
    // IMPORTANT: use reconcile_for_spsr_no_ticket here because we never called
    // enter_kernel — a normal reconcile would leak a ticket.
    #[cfg(kernel_smp_shared)]
    if interrupted_el0
        && crate::smp_shared::sched_bklfree_el0_enabled()
        && matches!(irq_opt, Some(i) if i == crate::gic::SGI_SCHEDULER)
    {
        let new_sp =
            akuma_exec::threading::sgi_scheduler_handler_with_sp(crate::gic::SGI_SCHEDULER, current_sp);
        let final_sp = if new_sp != 0 { new_sp } else { current_sp };
        // SAFETY: `final_sp` is a live IRQ trap frame; SPSR sits at a fixed offset.
        let spsr = unsafe {
            core::ptr::read_volatile((final_sp as usize + IRQ_FRAME_SPSR_OFFSET) as *const u64)
        };
        akuma_exec::bkl::reconcile_for_spsr_no_ticket(spsr);
        irq_eret_poison_check(final_sp, new_sp != 0);
        return new_sp;
    }

    // BKL-free device-IRQ dispatch (Phase 7a, docs/archive/BKL_PHASE7_AUDIT.md §2.3/§5).
    // The timer is the only device IRQ this kernel registers
    // (`irq::register_handler(27, timer::timer_irq_handler)`), and its handler
    // (`akuma_exec::alarms::on_timer_interrupt`'s alarm queue, the preemption watchdog, the
    // scheduler-SGI trigger) no longer touches anything the BKL alone protects: the
    // alarm queue has its own `Spinlock`, the watchdog reads are per-thread atomics, and
    // `trigger_sgi_self`/GIC ack/EOI are raw MMIO. Unlike the M5c fast path above, this
    // has no context switch and never calls `enter_kernel`, so there is nothing to
    // reconcile — the interrupted thread's BKL hold state (held or not, EL0 or EL1) is
    // left completely untouched by this excursion.
    #[cfg(all(kernel_smp_shared, kernel_no_bkl_irq))]
    if crate::smp_shared::irq_bkl_drop_enabled()
        && let Some(irq) = irq_opt
        && irq != crate::gic::SGI_SCHEDULER
    {
        crate::irq::dispatch_irq(irq);
        crate::gic::end_of_interrupt(irq);
        return 0;
    }

    // Device IRQ, or a scheduler SGI that preempted EL1 (BKL held): run holding the BKL.
    akuma_exec::bkl::enter_kernel();
    // Profiler bookkeeping only (no-op unless `bkl-profile` is on): the IRQ dispatch is a
    // TRANSIENT excursion that does not belong to the interrupted thread, so it stamps the
    // per-core sampling cache alone and leaves the interrupted thread's own per-thread tag
    // intact. Without this the core would stay mislabeled "irq/sched" for the rest of the
    // syscall it interrupted, attributing peer contention against the *remainder* of a long
    // syscall to the brief IRQ instead.
    #[cfg(kernel_smp_shared)]
    akuma_exec::sync::set_core_tag_transient(
        akuma_exec::bkl::current_core_id(),
        akuma_exec::sync::HOLD_TAG_IRQ,
    );

    let new_sp = if let Some(irq) = irq_opt {
        if irq == crate::gic::SGI_SCHEDULER {
            akuma_exec::threading::sgi_scheduler_handler_with_sp(irq, current_sp)
        } else {
            // Normal device IRQ: dispatch then EOI.
            crate::irq::dispatch_irq(irq);
            crate::gic::end_of_interrupt(irq);
            0
        }
    } else {
        0
    };

    // End of the transient excursion: point the sampling cache back at whichever thread
    // this core is about to run. ONE rule covers both outcomes, which is why the old
    // `if new_sp == 0 { restore saved tag }` special case is gone — after a switch
    // (`new_sp != 0`) `current_thread_id()` is the INCOMING thread and we install its own
    // tag (previously it inherited `irq/sched` and kept it for the rest of its syscall);
    // with no switch it is the interrupted thread and we reinstall the tag it never lost.
    #[cfg(kernel_smp_shared)]
    akuma_exec::sync::load_thread_tag_to_core(
        akuma_exec::bkl::current_core_id(),
        akuma_exec::threading::current_thread_id(),
    );

    #[cfg(kernel_smp_shared)]
    {
        // The asm erets into `new_sp`'s frame after a switch, else the interrupted
        // `current_sp` frame. Reconcile the BKL to that frame's target EL.
        let final_sp = if new_sp != 0 { new_sp } else { current_sp };
        // SAFETY: `final_sp` is a live IRQ trap frame; SPSR sits at a fixed offset.
        let spsr = unsafe {
            core::ptr::read_volatile((final_sp as usize + IRQ_FRAME_SPSR_OFFSET) as *const u64)
        };
        akuma_exec::bkl::reconcile_for_spsr(spsr);
    }
    {
        let final_sp = if new_sp != 0 { new_sp } else { current_sp };
        irq_eret_poison_check(final_sp, new_sp != 0);
    }
    new_sp
}

/// Landing pad for EL1 fault recovery.
///
/// After an EC=0x25 data abort from kernel code, ELR is redirected here so
/// that ERET doesn't return to the middle of the faulting instruction sequence
/// (which would immediately fault again, causing an infinite loop).
///
/// The process is already marked Zombie before we land here. We call
/// `return_to_kernel` to properly close all file descriptors (sockets, pipes,
/// etc.) and free the process's address space.  Without this, each EL1-fault
/// crash leaks all socket slots in the 128-slot socket table, causing later
/// `bun install` runs to fail to allocate UDP sockets for DNS.
///
/// Safety: ERET from an EL1 exception restores SPSR_EL1 which had EL1 mode
/// bits, so this function runs at EL1 and can safely call kernel functions.
#[unsafe(no_mangle)]
extern "C" fn el1_fault_recovery_pad() {
    akuma_exec::process::return_to_kernel_from_fault(-14);
}

/// Fast-path CoW write fault resolver for EL1 kernel code.
///
/// Explain a *write* permission fault that reached SIGSEGV instead of resolving.
///
/// A write to a mapped-but-read-only user page has exactly two recovery paths in
/// the EL0 data-abort arm: the CoW break (taken when `cow_ref_get(pa) > 0`) and the
/// lazy-region permission upgrade behind it. When neither fires the process dies
/// with nothing in the `[Fault]` block naming *which* one declined, and the
/// candidate causes are indistinguishable after the fact:
///
/// - `cow_ref=0`  — the RO page carries no CoW reference, so the break is skipped.
///   This is what a shared `file_page_cache` frame looks like once its reference has
///   been dropped, and what an ELF segment page mapped RO outside the cache looks
///   like always.
/// - `cow_ref>0` — the break was eligible and still failed: PMM exhaustion
///   (`alloc_page_zeroed` → `None`) or no `lookup_process_shared(as_owner)`. `free=`
///   separates those two.
/// - `lazy_self=None lazy_owner=Some(..)` — the upgrade at the bottom of the
///   permission-fault block looks the region up under `pid` while `mmap_regions`
///   live on the address-space owner, so a CLONE_VM thread misses a region that is
///   really there.
///
/// Cheap enough to leave unconditional: it runs once, on the way to a fatal signal.
fn print_write_perm_fault_diag(far: u64, iss: u64, pid: u32, as_owner: u32) {
    // DFSC[5:2] == 0b0011 (permission fault) with WnR set. Anything else reached
    // SIGSEGV by a path this diagnostic cannot explain.
    if (iss & 0x3C) != 0x0C || (iss & (1 << 6)) == 0 {
        return;
    }
    let far_usize = far as usize;
    let page_va = far_usize & !0xFFF;
    let ttbr0: u64;
    unsafe { core::arch::asm!("mrs {}, TTBR0_EL1", out(reg) ttbr0); }
    let l0_addr = (ttbr0 & 0x0000_FFFF_FFFF_F000) as usize;
    let l0_ptr = akuma_exec::mmu::phys_to_virt(l0_addr) as *const u64;
    let pa = akuma_exec::mmu::translate_user_va(l0_ptr, page_va).map(|p| p & !0xFFF);
    let cow = pa.map_or(0, crate::pmm::cow_ref_get);
    // `.0` is the region's user flags; `is_none` on it is what gates the upgrade.
    let lazy_self = akuma_exec::process::lazy_region_lookup_for_page_fault(pid, far_usize).map(|t| t.0);
    let lazy_owner =
        akuma_exec::process::lazy_region_lookup_for_page_fault(as_owner, far_usize).map(|t| t.0);
    let eager = akuma_exec::process::eager_region_flags_for_page_fault(pid, far_usize);
    // The live PTE, decoded. `ap_rw=true` here means the page table ALREADY grants
    // this write, so the access was legal and the SIGSEGV is spurious — the fault
    // was taken before some other thread repaired the page and is being judged
    // after. That is a different defect from "the permission is genuinely missing"
    // and the two are indistinguishable without this field.
    let pte = akuma_exec::mmu::user_pte_raw(l0_ptr, page_va);
    let ap_rw = pte.is_some_and(|p| {
        p & akuma_exec::mmu::flags::AP_MASK == akuma_exec::mmu::flags::AP_RW_ALL
    });
    let (_total, _alloc, free) = crate::pmm::stats();
    crate::safe_print!(255,
        "[WPF] pid={} as_owner={} va={:#x} pa={:#x} mapped={} cow_ref={} lazy_self={:#x} \
         lazy_owner={:#x} eager={:#x} pte={:#x} ap_rw={} have_owner={} free={}\n",
        pid, as_owner, page_va,
        pa.unwrap_or(0), pa.is_some(), cow,
        lazy_self.unwrap_or(u64::MAX), lazy_owner.unwrap_or(u64::MAX),
        eager.unwrap_or(u64::MAX), pte.unwrap_or(0), ap_rw,
        akuma_exec::process::lookup_process_shared(as_owner).is_some(),
        free);
}

/// Per-core count of successful exception-vector entries (sync EL1, sync EL0, IRQ —
/// anything that got far enough to run its Rust handler body). Indexed by
/// `bkl::current_core_id()`.
///
/// This exists to distinguish two failure shapes that look identical from the
/// outside (all cores pinned, ~400% host CPU, no console progress):
/// a `fault -> handler -> eret -> refault` loop keeps *entering* the handler over
/// and over (this counter climbs fast on that core), versus the page-table UAF
/// storm (docs/archive/PAGE_TABLE_UAF_BKL_STORM.md) where the vector's own first
/// instruction fetch fails — the handler is never entered at all, so this counter
/// on that core freezes solid while the other cores' counters keep climbing.
/// A frozen-vs-climbing split, read live via a debugger, is a harder proof of
/// "zero forward progress on this core" than byte-identical register snapshots.
pub static EXCEPTION_ENTRIES: [core::sync::atomic::AtomicU64; akuma_exec::threading::MAX_CORES] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    [INIT; akuma_exec::threading::MAX_CORES]
};

#[inline(always)]
fn note_exception_entry() {
    let core = akuma_exec::bkl::current_core_id() as usize;
    if core < EXCEPTION_ENTRIES.len() {
        EXCEPTION_ENTRIES[core].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
}

/// Per-INTID IRQ entry counts (INTIDs >= 63 pool in the last slot). Companion to
/// [`EXCEPTION_ENTRIES`]: that told us "~1M vector entries/s/core under llama
/// decode" without saying WHICH exception; this decomposes the IRQ share so a
/// screaming interrupt is attributable to its source (SGI 0 = scheduler, PPI 27
/// = CNTV tick, SPIs = virtio). Printed next to `[EXC]` in the heartbeat.
pub static IRQ_BY_INTID: [core::sync::atomic::AtomicU64; 64] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    [INIT; 64]
};

#[inline(always)]
fn note_irq_intid(intid: u32) {
    let slot = (intid as usize).min(IRQ_BY_INTID.len() - 1);
    IRQ_BY_INTID[slot].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// IRQ-vector entries whose GIC acknowledge came back "spurious" (IAR 1023).
/// Printed as `spurious=` on the `[IRQS]` heartbeat line.
pub static SPURIOUS_IRQS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// [`EXCEPTION_ENTRIES`] decomposed by vector class: 0 = sync EL0, 1 = sync
/// EL1, 2 = IRQ, 3 = default/unexpected. Under llama decode the per-core entry
/// rate is >1M/s while IRQs + syscalls + counted faults sum to <10K/s — this
/// split names which handler eats the difference. For sync EL0/EL1 the low
/// 6 bits of the companion `SYNC_EC_*` counters bucket ESR.EC (0x15 = SVC,
/// 0x24/0x25 = data abort, 0x20/0x21 = instr abort, 0x07 = FP/SIMD trap, ...).
pub static EXC_BY_CLASS: [core::sync::atomic::AtomicU64; 4] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    [INIT; 4]
};

/// ESR.EC histogram for sync-EL0 entries (index = EC, 6 bits).
pub static SYNC_EC_EL0: [core::sync::atomic::AtomicU64; 64] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    [INIT; 64]
};

/// ESR.EC histogram for sync-EL1 entries (index = EC, 6 bits).
pub static SYNC_EC_EL1: [core::sync::atomic::AtomicU64; 64] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    [INIT; 64]
};

#[inline(always)]
fn note_exc_class(class: usize) {
    EXC_BY_CLASS[class & 3].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// First 8 distinct trapped MSR/MRS encodings + hit counts (EC=0x18 storm
/// attribution). `.0` holds `key + 1` (0 = free slot) where key packs
/// direction | (crm<<1) | (crn<<5) | (op1<<9) | (op0<<12) | (op2<<16).
pub static MRS_TRAP_ENCODINGS: [(
    core::sync::atomic::AtomicU64,
    core::sync::atomic::AtomicU64,
); 8] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: (
        core::sync::atomic::AtomicU64,
        core::sync::atomic::AtomicU64,
    ) = (
        core::sync::atomic::AtomicU64::new(0),
        core::sync::atomic::AtomicU64::new(0),
    );
    [INIT; 8]
};

/// Write faults absorbed because the PTE already granted the write — the access
/// was legal by the time the handler looked, so the fault was stale.
pub static STALE_TLB_WRITE_FAULTS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Times the same thread came back to the same VA with the *same* PTE still
/// granting the write. Retrying did not clear it, so something other than a stale
/// view is at work and we decline rather than loop. Non-zero wants investigating.
pub static STALE_TLB_REPEATS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Last (VA, PTE) each thread absorbed, and how many times running. Per thread, so
/// two cores faulting on different pages never confuse each other. The PTE is part
/// of the key because a *different* PTE means real work happened in between (a CoW
/// break installed a new frame, an mprotect rewrote flags) — that is progress, not
/// a loop, and must not count toward the bound.
static STALE_TLB_LAST: [core::sync::atomic::AtomicUsize; akuma_exec::threading::types::MAX_THREADS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; akuma_exec::threading::types::MAX_THREADS];
static STALE_TLB_LAST_PTE: [core::sync::atomic::AtomicU64; akuma_exec::threading::types::MAX_THREADS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; akuma_exec::threading::types::MAX_THREADS];
static STALE_TLB_RUN: [core::sync::atomic::AtomicU32; akuma_exec::threading::types::MAX_THREADS] =
    [const { core::sync::atomic::AtomicU32::new(0) }; akuma_exec::threading::types::MAX_THREADS];

/// How many times one thread may absorb a write fault on the same VA with an
/// unchanged PTE before we stop absorbing. One retry is the expected case (the
/// instruction re-executes and succeeds); a run means the retry isn't taking.
const STALE_WRITE_FAULT_RETRY_LIMIT: u32 = 2;

/// Is this write fault already satisfied by the live page table?
///
/// A write permission fault says only what the CPU saw *when it took the fault*.
/// By the time this handler examines the address space, a sibling thread on
/// another core may already have repaired the page — most often by breaking CoW on
/// it, which replaces the frame and marks the PTE writable. The loser of that race
/// arrives holding a fault for a write that is now perfectly legal, and every
/// downstream repair path declines it: the CoW break sees `cow_ref == 0` (the
/// winner consumed the reference), and the region lookups find nothing for a VA
/// that never had an `mmap` record — an ELF `.data`/`.bss` page has none. The
/// process then dies on a write to its own global variable.
///
/// So: re-read the PTE and, if it grants EL0 write access, invalidate and let the
/// instruction re-execute. This is the page-fault re-check every SMP kernel needs;
/// Linux does the same thing under the page-table lock (`pte_same`).
///
/// Bounded by [`STALE_WRITE_FAULT_RETRY_LIMIT`] on (VA, PTE) so a fault that
/// genuinely cannot be cleared this way falls through to the normal repair instead
/// of spinning on fault → retry → fault.
fn stale_write_fault_absorbed(page_va: usize) -> bool {
    let ttbr0: u64;
    #[cfg(target_os = "none")]
    unsafe { core::arch::asm!("mrs {}, TTBR0_EL1", out(reg) ttbr0); }
    #[cfg(not(target_os = "none"))]
    { ttbr0 = 0; }
    if ttbr0 == 0 {
        return false;
    }
    let l0 = akuma_exec::mmu::phys_to_virt((ttbr0 & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
    let tid = akuma_exec::threading::current_thread_id();
    if !stale_write_fault_absorbed_in(l0, page_va, tid) {
        return false;
    }
    // The peer that repaired the page broadcasts its own invalidation, so this is
    // belt-and-braces — but the fault is already paid for, and a second `tlbi` is
    // far cheaper than a wrongly-fatal signal if any repair path ever forgets one.
    akuma_exec::mmu::flush_tlb_page(page_va);
    true
}

/// The decision half of [`stale_write_fault_absorbed`], against an explicit L0 and
/// thread id so the boot suite can drive it without a live TTBR0. Does the
/// bookkeeping and the counters; the caller does the TLB invalidation.
pub fn stale_write_fault_absorbed_in(l0: *const u64, page_va: usize, tid: usize) -> bool {
    use core::sync::atomic::Ordering;
    let Some(pte) = akuma_exec::mmu::user_pte_raw(l0, page_va) else { return false };
    if pte & akuma_exec::mmu::flags::AP_MASK != akuma_exec::mmu::flags::AP_RW_ALL {
        return false;
    }
    if tid >= akuma_exec::threading::types::MAX_THREADS {
        return false;
    }
    // Both `swap`s must run — short-circuiting the second would leave the stored
    // PTE stale and make the *next* call compare against the wrong baseline.
    let same_va = STALE_TLB_LAST[tid].swap(page_va, Ordering::Relaxed) == page_va;
    let same_pte = STALE_TLB_LAST_PTE[tid].swap(pte, Ordering::Relaxed) == pte;
    let run = if same_va && same_pte {
        STALE_TLB_RUN[tid].fetch_add(1, Ordering::Relaxed) + 1
    } else {
        STALE_TLB_RUN[tid].store(1, Ordering::Relaxed);
        1
    };
    if run > STALE_WRITE_FAULT_RETRY_LIMIT {
        STALE_TLB_REPEATS.fetch_add(1, Ordering::Relaxed);
        crate::safe_print!(160,
            "[TLB-STALE] va={:#x} still faulting after {} retries — PTE grants write but the \
             fault persists; falling through to the normal repair\n", page_va, run - 1);
        return false;
    }
    STALE_TLB_WRITE_FAULTS.fetch_add(1, Ordering::Relaxed);
    true
}

/// Forensic probe of the physical page behind a live user VA, printed at an
/// anomaly.
///
/// This exists for the cargo null-`Rc` defect (docs/archive/CARGO_HEAP_NULL_RC.md),
/// whose signature is a heap qword that reads back as zero with **no fault at the
/// moment of corruption**. The mechanism that produces that silently is a frame
/// returned to the PMM while a process still maps it: the next
/// `alloc_page_zeroed` wipes the page under its live owner. So the one question
/// worth asking at any page-level anomaly is whether the PA behind this PTE is
/// *simultaneously on the free list* — `free=true` is proof, not a hint, and
/// `last_free` then names the pid that released it.
///
/// `first_words` distinguishes the two downstream states that otherwise look
/// alike: a page whose content is intact but whose permissions were lost (an
/// accounting bug) versus one that has already been wiped (the frame is gone).
///
/// Runs only on anomaly paths (`EAGER-UPGRADE`, `WILD-DA`), never per-fault.
fn print_page_forensics(tag: &str, pid: u32, va: usize) {
    let page_va = va & !0xFFF;
    let ttbr0: u64;
    #[cfg(target_os = "none")]
    unsafe { core::arch::asm!("mrs {}, TTBR0_EL1", out(reg) ttbr0); }
    #[cfg(not(target_os = "none"))]
    { ttbr0 = 0; }
    let l0_addr = (ttbr0 & 0x0000_FFFF_FFFF_F000) as usize;
    let l0_ptr = akuma_exec::mmu::phys_to_virt(l0_addr) as *const u64;
    let Some(pa) = akuma_exec::mmu::translate_user_va(l0_ptr, page_va).map(|p| p & !0xFFF) else {
        crate::safe_print!(128, "[{}] pid={} va={:#x} UNMAPPED\n", tag, pid, page_va);
        return;
    };

    // Read through the kernel identity map, not the user VA: the point is to
    // inspect the frame regardless of what the user PTE currently permits.
    let words = unsafe {
        let p = akuma_exec::mmu::phys_to_virt(pa).cast::<u64>();
        [p.read_volatile(), p.add(1).read_volatile(),
         p.add(2).read_volatile(), p.add(3).read_volatile()]
    };
    // Distance, not the raw seq: "freed 40 frees ago" and "freed 300 000 frees
    // ago" are different findings, and only the first implicates this frame. With
    // no record at all there IS no distance — printing one computed against a
    // default seq of 0 yields a large number that reads exactly like a real
    // (innocent) age, so report the absence instead.
    let (tid_freed, free_age) = match crate::pmm::last_free_record(pa) {
        Some((tid, seq)) => (i64::from(tid), i64::from(crate::pmm::free_ledger_seq().wrapping_sub(seq))),
        None => (-1, -1),
    };
    let tracked = akuma_exec::process::lookup_process_shared(pid)
        .is_some_and(|p| p.address_space.tracks_user_frame(pa));
    crate::safe_print!(255,
        "[{}] pid={} va={:#x} pa={:#x} FREE={} cow_ref={} tracked={} \
         last_free=(tid={} age={}) [-1 = never freed] head={:#x},{:#x},{:#x},{:#x}\n",
        tag, pid, page_va, pa,
        crate::pmm::is_page_free(pa),
        crate::pmm::cow_ref_get(pa),
        tracked, tid_freed, free_age,
        words[0], words[1], words[2], words[3]);
    // The reference history is what separates "this page's count was legitimately
    // 0" from "someone decremented it once too often".
    crate::pmm::print_cow_history(pa);

    // How many eager regions claim this VA? The fault handler answers from the
    // first `Vec` match, so two overlapping regions mean the record it consulted
    // may describe a mapping that no longer exists — the current leading theory
    // for this anomaly. One region refutes it; more than one convicts.
    let regions = akuma_exec::process::eager_regions_containing(pid, page_va);
    crate::safe_print!(96, "  [REGIONS] va={:#x} claimed_by={}\n", page_va, regions.len());
    for (start, pages, flags) in regions.iter().take(4) {
        crate::safe_print!(128, "    start={:#x} pages={} flags={:#x}\n", start, pages, flags);
    }

    // What the PTE actually says. A write can permission-fault on a read-only page,
    // on a `PROT_NONE` guard page, and on a kernel-only page — three different
    // defects that a PA alone cannot tell apart, and the repair above grants the
    // write in all three cases.
    if let Some(pte) = akuma_exec::mmu::user_pte_raw(l0_ptr, page_va) {
        crate::safe_print!(160, "  [PTE] va={:#x} raw={:#x} ap={}\n",
            page_va, pte, akuma_exec::mmu::ap_name(pte));
    } else {
        crate::safe_print!(96, "  [PTE] va={:#x} no valid descriptor\n", page_va);
    }

    // A lazy region covering the same VA as an eager one is invisible to
    // `[REGIONS]` but decisive here: a `PROT_NONE` lazy region makes the fault
    // handler skip its own lazy arm and fall through to the eager repair, which
    // then promotes a page userspace asked to be unmapped.
    if let Some((flags, _src, start, size)) =
        akuma_exec::process::lazy_region_lookup_for_page_fault(pid, page_va)
    {
        crate::safe_print!(192,
            "  [LAZY] va={:#x} ALSO covered by lazy region start={:#x} size={:#x} flags={:#x} prot_none={}\n",
            page_va, start, size, flags, akuma_exec::mmu::user_flags::is_none(flags));
    } else {
        crate::safe_print!(96, "  [LAZY] va={:#x} no lazy region\n", page_va);
    }
}

/// Returns true if the fault was a CoW write permission fault, the CoW page was
/// successfully resolved (new frame allocated, old PA remapped), and the caller
/// should return immediately to retry the faulting instruction via ERET.
///
/// Called before the full EL1 debug dump so normal CoW faults produce no log noise.
fn try_resolve_el1_cow_fault() -> bool {
    // Read necessary system registers.
    let fault_esr: u64;
    let fault_far: u64;
    let fault_ttbr0: u64;
    let fault_pc: u64; // ELR_EL1 = faulting instruction address
    unsafe {
        core::arch::asm!("mrs {}, esr_el1",   out(reg) fault_esr);
        core::arch::asm!("mrs {}, far_el1",   out(reg) fault_far);
        core::arch::asm!("mrs {}, ttbr0_el1", out(reg) fault_ttbr0);
        core::arch::asm!("mrs {}, elr_el1",   out(reg) fault_pc);
    }

    let ec = (fault_esr >> 26) & 0x3F;
    let iss = fault_esr & 0x01FF_FFFF;
    let dfsc = iss & 0x3F;

    // Must be: data abort from EL1 (0x25) + permission fault L3 (0x0F) + write (WnR=bit6)
    // + kernel code is executing (ELR in kernel text range).
    // No FAR range check — user VA space spans 0..512GB; translate_user_va() handles
    // unmapped/kernel addresses by returning None.
    if ec != 0x25 || dfsc != 0x0F || (iss & (1 << 6)) == 0 { return false; }
    if !(akuma_exec::mmu::is_kernel_text(fault_pc as usize)) { return false; }

    let l0_addr = (fault_ttbr0 & 0x0000_FFFF_FFFF_F000) as usize;
    let l0_ptr = akuma_exec::mmu::phys_to_virt(l0_addr) as *const u64;
    let page_va = fault_far as usize & !(0xFFF);

    let old_pa = match akuma_exec::mmu::translate_user_va(l0_ptr, page_va) {
        Some(pa) => pa & !(0xFFF),
        None => return false,
    };

    if crate::pmm::cow_ref_get(old_pa) == 0 { return false; }

    let new_frame = match crate::pmm::alloc_page_zeroed() {
        Some(f) => f,
        None => return false, // OOM — fall through to kill-process path
    };

    // F2 FIXED 2026-08-13: resolve the **address-space owner**, not the current pid —
    // the same resolution `ensure_cow_page_writable` and the EL0 arm already use.
    //
    // For a single-threaded process the two are the same pid, so this changes nothing.
    // For a `CLONE_VM` worker they differ: the worker has its own `Process` slot and
    // pid while the address space, and every frame tracked against it, belongs to the
    // thread-group leader. Resolving the worker leaked **two frames per kernel-side
    // CoW break** (`COW_PILE_AUDIT.md` §4 F2), because `new_shared` gives each sharer
    // its own empty `user_frames` map that its `Drop` never frees:
    //
    //   - `track_user_frame(new_frame)` landed on the worker's map → never freed;
    //   - `remove_user_frame(old_pa)` missed a map that never held it → returned
    //     false → the `released_last_va` gate correctly suppressed `cow_ref_dec` →
    //     `old_pa` kept an elevated refcount forever.
    //
    // It also took the worker's own `as_lock`, which no fault handler waits on, so the
    // critical section excluded nothing — a concurrent EL0 break on the leader's
    // `as_lock` ran straight through it. Both are fixed by naming the right owner.
    //
    // No explicit `read_current_pid` fallback here: `address_space_owner_pid_for_fault`
    // already ends its own chain with it (`children.rs:1053`), so chaining another one
    // would be unreachable code in a fault handler.
    //
    // It resolves through the **live** TTBR0 while `old_pa` above was translated
    // through the snapshotted `fault_ttbr0`. Those are the same value here — the fault
    // and this handler run on one thread and nothing between them switches address
    // space — and this is the resolution `ensure_cow_page_writable` and the EL0 arm
    // already rely on.
    let pid = akuma_exec::process::address_space_owner_pid_for_fault().unwrap_or(0);
    if let Some(owner) = akuma_exec::process::lookup_process_shared(pid) {
        complete_cow_break(CowRemap::TakingAsLock(owner), page_va, old_pa, new_frame);
        true // Caller returns; ERET retries the faulting instruction
    } else {
        // No process owner found: cannot remap — free the new frame to avoid a leak.
        // Do NOT cow_ref_dec: the page is still mapped RO with the original refcount.
        // Returning false lets the EL1 handler kill the process via the normal path.
        crate::pmm::free_page(new_frame);
        false
    }
}

/// Fast-path resolver for an EL1 kernel data abort caused by touching a LAZY
/// (not-yet-mapped) user page during a kernel→user copy (`copy_to_user` /
/// `copy_from_user`, or their `Prefault::No` forms) — e.g. the rump sysproxy `copyout`
/// writing a DNS answer
/// into an *unmodified* client's demand-paged receive buffer. The kernel copy is
/// the first touch of the buffer's later page(s), so the byte-copy loop takes a
/// translation fault at the page boundary. Without this the registered user-copy
/// fault handler turns it into EFAULT and the copy is silently truncated at the
/// boundary (this is what dropped the tail — the terminal A records — of DNS
/// answers over the rump stack, so `getaddrinfo` failed). Here we demand-page the
/// lazy anon page and return `true` so ERET retries the faulting byte and the copy
/// continues across the boundary — exactly how an EL0 touch of the same page would
/// be handled. (On a single kernel the BSP demo used the `hijack.so` userspace
/// path, which writes the buffer in-process and never hits kernel `copy_to_user`,
/// so this only surfaced with the kernel sysproxy path.)
///
/// Self-gating for safety: only a *translation* fault (page absent — a permission/
/// CoW fault is `try_resolve_el1_cow_fault`) from kernel code, on a page that lies
/// in a registered lazy zero-fill anon region ([`ensure_user_page_mapped`]), is
/// resolved. If the page is already mapped (yet still faulting) we return `false`
/// so we can't spin retrying. Anything else falls through to the kill path.
fn try_resolve_el1_user_copy_lazy_fault() -> bool {
    let fault_esr: u64;
    let fault_far: u64;
    let fault_pc: u64;
    unsafe {
        core::arch::asm!("mrs {}, esr_el1", out(reg) fault_esr);
        core::arch::asm!("mrs {}, far_el1", out(reg) fault_far);
        core::arch::asm!("mrs {}, elr_el1", out(reg) fault_pc);
    }
    let ec = (fault_esr >> 26) & 0x3F;
    let dfsc = fault_esr & 0x3F;
    // EL1 data abort (0x25) + translation fault (DFSC 0x04..=0x07 = page absent, L0..L3).
    if ec != 0x25 || !(0x04..=0x07).contains(&dfsc) {
        return false;
    }
    // A real copy_to_user/from_user is executing in kernel text.
    if !(akuma_exec::mmu::is_kernel_text(fault_pc as usize)) {
        return false;
    }
    let page_va = fault_far as usize & !0xFFF;
    // Already mapped but still faulting → not a lazy miss; don't claim it (avoids a
    // retry loop). `ensure_user_page_mapped` self-gates on a lazy zero-fill region.
    if akuma_exec::mmu::is_current_user_page_mapped(page_va) {
        return false;
    }
    let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    if ensure_user_page_mapped(pid, page_va) {
        akuma_exec::mmu::flush_tlb_page(page_va);
        true // ERET retries the faulting byte; the copy continues across the boundary
    } else {
        false
    }
}

/// Synchronous exception handler from EL1 (kernel mode)
/// Uses static buffers to avoid heap allocation during crash
///
/// `saved_regs` points at the block the vector pushed:
/// `[x2, x3, x0, x1, x29, x30]` (see `sync_el1_handler` asm). x30 in particular
/// names the CALLER when the fault is a wild branch (`blr` through a corrupt
/// pointer leaves the call site + 4 in LR).
#[unsafe(no_mangle)]
extern "C" fn rust_sync_el1_handler(saved_regs: *const u64) {
    note_exception_entry();
    note_exc_class(1);
    {
        let esr: u64;
        // SAFETY: reading the syndrome register has no side effects; this handler
        // owns the trap until it reads it (IRQs still masked at entry).
        unsafe { core::arch::asm!("mrs {}, esr_el1", out(reg) esr) };
        SYNC_EC_EL1[((esr >> 26) & 0x3F) as usize]
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
    // Quick CoW pre-check: avoid the full debug dump for expected CoW write faults.
    // EC=0x25 + DFSC=0x0F (permission fault L3) + WnR + user VA → almost certainly CoW.
    // Quick CoW pre-check: resolve CoW write faults before full debug dump.
    // EC=0x25 + DFSC=0x0F (permission fault L3) + WnR + user VA → CoW from kernel code.
    if try_resolve_el1_cow_fault() {
        return;
    }
    // Demand-page a lazy user page touched by a kernel→user copy (sysproxy copyout,
    // etc.) and retry, instead of EFAULT-truncating the copy at the page boundary.
    if try_resolve_el1_user_copy_lazy_fault() {
        return;
    }

    // NOTE: We intentionally do NOT check get_user_copy_fault_handler() here
    // (before the debug dump).  That function acquires POOL lock inside
    // with_irqs_disabled.  If an EL1 data abort fires while POOL lock is
    // already held (e.g. during context switch), the lock acquisition would
    // deadlock.  The existing check at line ~1461 (after the debug dump,
    // inside the EC=0x25 branch) has the same risk but is only reached for
    // actual data aborts — moving it earlier increases the window.
    // The debug dump noise for user-copy faults is acceptable.

    // Read ESR_EL1 to determine exception type
    let esr: u64;
    let elr: u64;
    let far: u64;
    let spsr: u64;
    let ttbr0: u64;
    let ttbr1: u64;
    let sp: u64;
    let sp_el0: u64;
    unsafe {
        core::arch::asm!("mrs {}, esr_el1", out(reg) esr);
        core::arch::asm!("mrs {}, elr_el1", out(reg) elr);
        core::arch::asm!("mrs {}, far_el1", out(reg) far);
        core::arch::asm!("mrs {}, spsr_el1", out(reg) spsr);
        core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr0);
        core::arch::asm!("mrs {}, ttbr1_el1", out(reg) ttbr1);
        core::arch::asm!("mov {}, sp", out(reg) sp);
        core::arch::asm!("mrs {}, sp_el0", out(reg) sp_el0);
    }

    let ec = (esr >> 26) & 0x3F;
    let iss = esr & 0x1FFFFFF;
    let tid = akuma_exec::threading::current_thread_id();

    safe_print!(256, "[Exception] Sync from EL1: EC={ec:#x}, ISS={iss:#x}\n");
    safe_print!(256, "  ELR={elr:#x}, FAR={far:#x}, SPSR={spsr:#x}\n");
    safe_print!(256, "  Thread={tid}, TTBR0={ttbr0:#x}, TTBR1={ttbr1:#x}\n");
    safe_print!(256, "  SP={sp:#x}, SP_EL0={sp_el0:#x}\n");
    // Saved-register block from the vector: [x2, x3, x0, x1, x29, x30].
    // x30 names the caller when ELR is a wild branch target (blr through a
    // corrupt pointer leaves call site + 4 in LR).
    if !saved_regs.is_null() {
        let (r_x2, r_x3, r_x0, r_x1, r_x29, r_x30) = unsafe {
            (saved_regs.read(), saved_regs.add(1).read(), saved_regs.add(2).read(),
             saved_regs.add(3).read(), saved_regs.add(4).read(), saved_regs.add(5).read())
        };
        safe_print!(256, "  x0={r_x0:#x} x1={r_x1:#x} x2={r_x2:#x} x3={r_x3:#x}\n");
        safe_print!(256, "  x29={r_x29:#x} x30(LR)={r_x30:#x} core={}\n",
            akuma_exec::bkl::current_core_id());
    }

    // Try to read the faulting instruction (if ELR is in kernel range)
    if (0x4000_0000..0x8000_0000).contains(&elr) {
        let instr = unsafe { *(elr as *const u32) };
        safe_print!(256, "  Instruction at ELR: {instr:#010x}\n");

        // Decode ARM64 load/store instruction to find base register
        // LDR/STR format: opc[31:30] | 111 | V[26] | 00 | opc2[23:22] | imm9 | op[11:10] | Rn[9:5] | Rt[4:0]
        // Or: opc[31:30] | 111 | V[26] | 01 | opc2[23:22] | imm12[21:10] | Rn[9:5] | Rt[4:0]
        let rn = ((instr >> 5) & 0x1F) as usize;
        let rt = (instr & 0x1F) as usize;
        safe_print!(256, "  Likely: Rn(base)=x{rn}, Rt(dest)=x{rt}\n");
    }
    
    // Check if FAR is in user space (below 0x40000000)
    if far < 0x4000_0000 {
        safe_print!(128, "  WARNING: Kernel accessing user-space address!\n");
        safe_print!(128, "  This suggests stale TTBR0 or dereferencing user pointer from kernel.\n");
    }

    // Recovery: if this is a data abort (EC=0x25) caused by writing/reading a bad
    // address while executing kernel (syscall) code, kill only the offending process
    // instead of halting the kernel.  This guards against validate_user_ptr letting a
    // kernel address slip through.
    if ec == 0x25 {
        let in_kernel_code = akuma_exec::mmu::is_kernel_text(elr as usize);
        if in_kernel_code {
            // Check if thread has a registered fault handler for user copy operations
            let fault_handler = akuma_exec::threading::get_user_copy_fault_handler();
            if fault_handler != 0 {
                // Redirect ELR to the recovery handler
                // This allows copy_from_user/copy_to_user to return EFAULT safely
                unsafe {
                    core::arch::asm!("msr elr_el1, {}", in(reg) fault_handler);
                }
                // Clear the handler to prevent infinite loops if the recovery code itself faults
                akuma_exec::threading::set_user_copy_fault_handler(0);
                return;
            }

            if (0x4000_0000..0x8000_0000).contains(&far) {
                safe_print!(256, "  HINT: FAR={far:#x} is in kernel identity-mapped RAM range.\n");
                safe_print!(256, "  Likely cause: phys_to_virt() write to a physical page whose VA\n");
                safe_print!(256, "  is not mapped in the current user page tables (TTBR0).\n");
            }
            safe_print!(256, "  EC=0x25 in kernel code — killing current process (EFAULT)\n");
            if let Some(proc) = akuma_exec::process::current_process_shared() {
                safe_print!(256, "  Killing PID {} ({})\n", proc.pid, proc.name);
                let l0_phys = proc.address_space.l0_phys();
                let pid = proc.pid;
                akuma_exec::process::with_current_process(|p| {
                    p.exited = true;
                    p.exit_code = -14; // EFAULT
                    p.state = akuma_exec::process::ProcessState::Zombie(-14);
                });
                akuma_exec::process::kill_thread_group(pid, l0_phys, -14);
                crate::syscall::proc::notify_child_channel_exited_pub(pid, -14);
                crate::syscall::proc::vfork_complete(pid);
            }
            // Redirect ELR to the recovery landing pad so that ERET does NOT
            // return into the middle of the faulting instruction sequence.
            // Skipping by +4 would just execute the next instruction which
            // likely uses the same corrupt register and faults again, causing
            // an infinite fault loop (observed as repeated EC=0x25 with
            // FAR=0x1 as the cascade drifts through garbage code).
            // The landing pad yields in a loop; the scheduler stops dispatching
            // this thread once cleanup_terminated() recycles the slot.
            unsafe {
                let pad = el1_fault_recovery_pad as *const () as usize as u64;
                core::arch::asm!("msr elr_el1, {}", in(reg) pad);
            }
            return;
        }
    }

    // Check for page table corruption on translation table walk faults
    let dfsc = iss & 0x3F;
    if dfsc == 0x21 || dfsc == 0x22 || dfsc == 0x23 {
        // External abort on translation table walk (level 1/2/3)
        safe_print!(128, "  PAGE TABLE WALK FAULT - checking page table integrity:\n");
        
        // Get expected boot TTBR0
        let boot_ttbr0 = akuma_exec::mmu::get_boot_ttbr0();
        safe_print!(256, "    Expected boot_ttbr0: {boot_ttbr0:#x}\n");
        safe_print!(256, "    Current TTBR0:       {ttbr0:#x}\n");

        if ttbr0 != boot_ttbr0 {
            safe_print!(64, "    WARNING: TTBR0 mismatch!\n");
        }

        // Read L0[0] entry to check if it points to valid L1
        let l0_base = ttbr0 & !0xFFF; // Mask off ASID etc
        let l0_entry = unsafe { *(l0_base as *const u64) };
        safe_print!(256, "    L0[0] entry: {l0_entry:#018x}\n");

        // Check if L0[0] looks valid (should be table descriptor)
        let is_valid = (l0_entry & 0x1) == 1;
        let is_table = (l0_entry & 0x2) == 2;
        let l1_addr = l0_entry & 0x0000_FFFF_FFFF_F000;
        safe_print!(256, "    L0[0]: valid={is_valid}, table={is_table}, L1_addr={l1_addr:#x}\n");

        // Expected L1 address should be boot_ttbr0 + 8192 (2 pages)
        let expected_l1 = boot_ttbr0 + 8192;
        safe_print!(256, "    Expected L1 addr: {expected_l1:#x}\n");

        if l1_addr != expected_l1 {
            safe_print!(128, "    WARNING: L1 address mismatch - page table corrupted!\n");
        }

        // Now read L1[0] to check the device memory block entry
        if is_valid && is_table && (0x4000_0000..0x8000_0000).contains(&l1_addr) {
            let l1_entry = unsafe { *(l1_addr as *const u64) };
            safe_print!(256, "    L1[0] entry: {l1_entry:#018x}\n");

            // L1[0] should be a 1GB block descriptor for device memory
            // Valid block: bits[1:0] = 01, bits[47:30] = physical address
            let is_l1_valid = (l1_entry & 0x1) == 1;
            let is_block = (l1_entry & 0x2) == 0; // Block, not table
            let block_addr = l1_entry & 0x0000_FFFF_C000_0000;
            safe_print!(256, "    L1[0]: valid={is_l1_valid}, block={is_block}, phys_addr={block_addr:#x}\n");

            // L1[0] should point to physical 0 (device memory)
            if !is_l1_valid {
                safe_print!(64, "    WARNING: L1[0] is INVALID!\n");
            }
            if block_addr != 0 {
                safe_print!(64, "    WARNING: L1[0] block address wrong!\n");
            }
        }
    }
    
    // Log memory stats for debugging
    log_memory_stats_on_crash(tid, sp, sp_el0);

    // Halt on kernel exceptions - they indicate bugs
    loop {
        unsafe {
            core::arch::asm!("wfe");
        }
    }
}

/// Log comprehensive memory stats when a crash occurs
/// Uses static buffer to avoid heap allocations during crash
fn log_memory_stats_on_crash(tid: usize, kernel_sp: u64, user_sp: u64) {
    use core::fmt::Write;
    let mut w = crate::console::StackWriter::<256>::new();
    
    safe_print!(64, "\n=== Memory Stats at Crash ===\n");
    
    // Kernel heap stats
    let heap_stats = crate::allocator::stats();
    safe_print!(256, "  Heap: {}/{} bytes used ({} allocs, peak={})\n",
        heap_stats.allocated,
        heap_stats.heap_size,
        heap_stats.allocation_count,
        heap_stats.peak_allocated
    );

    // PMM stats
    let pmm_free = crate::pmm::free_count();
    let pmm_total = crate::pmm::total_count();
    safe_print!(256, "  PMM: {}/{} pages free ({} KB / {} KB)\n",
        pmm_free, pmm_total,
        pmm_free * 4, pmm_total * 4
    );

    // Frame tracking stats if enabled
    if let Some(frame_stats) = crate::pmm::tracking_stats() {
        safe_print!(256, "  Frames: kernel={}, user_pt={}, user_data={}, elf={}\n",
            frame_stats.kernel_count,
            frame_stats.user_page_table_count,
            frame_stats.user_data_count,
            frame_stats.elf_loader_count
        );
    }

    // Thread stack info
    let (thread_count, running, terminated) = akuma_exec::threading::thread_stats();
    safe_print!(256, "  Threads: {thread_count} total, {running} running, {terminated} terminated\n");

    // Current thread's kernel stack info
    if let Some(stack_info) = akuma_exec::threading::get_thread_stack_info(tid) {
        let kernel_stack_used = if kernel_sp >= stack_info.0 as u64 && kernel_sp <= stack_info.1 as u64 {
            stack_info.1 - kernel_sp as usize
        } else {
            0 // SP outside expected range
        };
        safe_print!(256, "  Thread {} kernel stack: base={:#x}, top={:#x}\n",
            tid, stack_info.0, stack_info.1
        );
        safe_print!(256, "    SP={kernel_sp:#x}, used={kernel_stack_used} bytes\n");
        if kernel_sp < stack_info.0 as u64 || kernel_sp > stack_info.1 as u64 {
            safe_print!(128, "  WARNING: Kernel SP outside thread's stack bounds!\n");
        }
    }

    // User process info (if any)
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        let mem = &proc.memory;
        let stack_size = mem.stack_top - mem.stack_bottom;
        let stack_used = if user_sp >= mem.stack_bottom as u64 && user_sp < mem.stack_top as u64 {
            mem.stack_top - user_sp as usize
        } else {
            0 // SP outside expected range (might be corrupted)
        };
        let heap_used = proc.brk.saturating_sub(proc.initial_brk);
        let mmap_used = mem.next_mmap.load(core::sync::atomic::Ordering::Relaxed).saturating_sub(0x1000_0000);

        // Print in smaller chunks to fit in static buffer
        safe_print!(256, "  Process PID={} '{}'\n", proc.pid, proc.name);

        safe_print!(256, "    Stack: {:#x}-{:#x} ({} KB)\n",
            mem.stack_bottom, mem.stack_top, stack_size / 1024
        );

        // Calculate percentage without floating point (integer percentage)
        let stack_pct = if stack_size > 0 { (stack_used * 100) / stack_size } else { 0 };
        safe_print!(256, "    SP_EL0={user_sp:#x}, used={stack_used} bytes ({stack_pct}%)\n");

        safe_print!(256, "    Heap: brk={:#x} (initial={:#x}), grown={} bytes\n",
            proc.brk, proc.initial_brk, heap_used
        );

        safe_print!(256, "    Mmap: next={:#x}, limit={:#x}, used={} bytes\n",
            mem.next_mmap.load(core::sync::atomic::Ordering::Relaxed), mem.mmap_limit, mmap_used
        );

        // Leak attribution: how many frames this process tracks vs the VA it
        // mapped, and the global per-site demand-paging page tally.
        safe_print!(256, "    Tracked(cur pid={}): user_frames={} refs={} page_tables={}\n",
            proc.pid,
            proc.address_space.user_frame_count(),
            proc.address_space.user_frame_total_refs(),
            proc.address_space.page_table_frame_count(),
        );
        if let Some(owner) = akuma_exec::process::lookup_process_shared(proc.tgid) {
            safe_print!(256, "    Tracked(owner tgid={}): user_frames={} refs={} page_tables={}\n",
                proc.tgid,
                owner.address_space.user_frame_count(),
                owner.address_space.user_frame_total_refs(),
                owner.address_space.page_table_frame_count(),
            );
        }
        let _ = write!(w, "    DP pages (global): ");
        crate::pmm::dp_counters_line(&mut w);
        let _ = writeln!(w);
        w.flush();

        if user_sp < mem.stack_bottom as u64 {
            safe_print!(128, "    WARNING: User SP below stack bottom - STACK OVERFLOW!\n");
        } else if user_sp >= mem.stack_top as u64 {
            safe_print!(128, "    WARNING: User SP above stack top - SP corrupted!\n");
        }
    } else {
        safe_print!(64, "  No current user process\n");
    }
    
    safe_print!(64, "=============================\n");
}

/// Is `instr` an AArch64 `SVC` instruction (any immediate)?
///
/// Encoding: `SVC #imm16` = `0b11010100_000_iiiiiiiiiiiiiiii_00001`. The opcode
/// bits (everything but the 16-bit immediate) are fixed at `0xD4000001`, so
/// masking off the immediate field (`0xFFE0001F`) and comparing to `0xD4000001`
/// recognises `svc` for any `imm16`. Used by the stale-I-cache spurious-SVC
/// guard and the >500 JIT-replay workaround in `rust_sync_el0_handler`: at an
/// `EC_SVC64` trap the (cache-coherent) instruction at `ELR-4` must satisfy
/// this, or the executed `svc` came from a stale I-cache line.
pub const fn is_aarch64_svc(instr: u32) -> bool {
    (instr & 0xFFE0_001F) == 0xD400_0001
}

/// Total EC_SVC64 traps whose instruction at ELR-4 was readable and
/// definitively NOT an `svc` (phantom SVC), counted once per trap (replays and
/// give-ups alike). Healthy runs keep this at 0 — the boot self-tests assert
/// it (see `process_tests`) and stress/acceptance runs should re-check it: a
/// nonzero count means syndrome misclassification (or stale I-cache) is back.
static SPURIOUS_SVC_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Read the phantom-SVC counter (see [`SPURIOUS_SVC_COUNT`]).
// Consumed by the boot self-tests, which `no-tests`/size-profile builds omit.
#[cfg_attr(not(kernel_tests), allow(dead_code))]
pub fn spurious_svc_count() -> u64 {
    SPURIOUS_SVC_COUNT.load(core::sync::atomic::Ordering::Relaxed)
}

/// Synchronous exception handler from EL0 (user mode)
/// Returns the syscall return value, or doesn't return if process exits
#[unsafe(no_mangle)]
/// Syscall / EL0-fault entry from the vector asm. Wraps the real handler with the
/// Big Kernel Lock (real shared-kernel SMP): a thread executing kernel code on behalf
/// of an EL0 trap holds the BKL for the whole excursion, so its shared-state access
/// (process table, VFS, net, page tables) is serialized against other cores. No-op
/// unless `cfg(kernel_smp_shared)` (see `akuma_exec::bkl`). The IRQ/scheduler path's
/// BKL reconciliation is added in M2; here the syscall thread may still be preempted
/// mid-excursion, which is correct on the single active core of M0/M1.
/// `esr`/`far` are the trap syndrome snapshotted by the vector asm at exception
/// entry, while PSTATE.I was still masked. They MUST be used instead of live
/// `mrs esr_el1`/`mrs far_el1` reads anywhere in this call chain: the BKL spin
/// below runs with IRQs enabled, so this thread can be preempted (or even resume
/// on another core) and the live syndrome registers then belong to someone
/// else's trap. Same for ELR_EL1 — use the frame's saved copy.
extern "C" fn rust_sync_el0_handler(frame: *mut UserTrapFrame, esr: u64, far: u64) -> u64 {
    // Outermost span for `read-profile` (ZST otherwise): started before the BKL
    // acquire and the entry tripwires, so `exc - hs` names everything this
    // wrapper does around the dispatch. See `crate::syscall::utils::read_profile`.
    let rp_span = crate::syscall::utils::read_profile::Span::new();
    note_exception_entry();
    note_exc_class(0);
    SYNC_EC_EL0[((esr >> 26) & 0x3F) as usize]
        .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    // Per-syscall BKL opt-out (Phase 7f, BKL_FINE_GRAINED_LOCKING_PLAN.md §7.3):
    // acquire the BKL UNLESS the trapped syscall's number is on the opt-out list.
    // The decision is LATCHED here and reused verbatim on every exit path below —
    // never re-read (the guard-latching rule in locking.md; a mid-syscall toggle
    // flip must not unbalance entry against exit). An opted-out excursion runs as
    // ONE open dropped-BKL window, opened after the tripwire and closed without a
    // re-acquire at exit, so an IRQ landing anywhere inside reconciles to
    // "released" via the ledger instead of silently re-acquiring-and-holding, and
    // the body's now-redundant carve-out guard nests as an inner (depth-2) window.
    // Faults (EC != SVC64) always take the lock. `frame` is this thread's own
    // kernel stack — reading x8 pre-acquire touches no shared state.
    #[cfg(kernel_smp_shared)]
    let bkl_optout = ((esr >> 26) & 0x3F) == esr::EC_SVC64
        && crate::smp_shared::syscall_bkl_optout(unsafe { (*frame).x8 });
    #[cfg(not(kernel_smp_shared))]
    let bkl_optout = false;
    if !bkl_optout {
        akuma_exec::bkl::enter_kernel();
    }
    // Tripwire: a thread entering from EL0 can have no open dropped-BKL window — the
    // guards live entirely inside one excursion. A nonzero depth here is a leak (an
    // abnormal unwind skipped a guard's destructor, or a recycled slot inherited state);
    // left in place it would make this excursion's IRQ epilogues silently RELEASE the
    // BKL mid-syscall. Heal it and say so. Must stay AHEAD of the opt-out window open
    // below so it can never mistake this excursion's own legitimate window for a leak.
    #[cfg(kernel_smp_shared)]
    if akuma_exec::bkl::in_dropped_window() {
        let leaked = akuma_exec::bkl::reset_dropped_windows();
        if leaked != 0 {
            STALE_WINDOW_HEALS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            static STALE_WINDOW_TRACE: core::sync::atomic::AtomicU32 =
                core::sync::atomic::AtomicU32::new(0);
            if STALE_WINDOW_TRACE.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 8 {
                crate::safe_print!(
                    96,
                    "[BKL] stale dropped-window depth {} healed at EL0 entry (tid={})\n",
                    leaked,
                    akuma_exec::threading::current_thread_id(),
                );
            }
        }
    }
    // Open the opted-out excursion's window. `dropped_window_open`'s `leave_kernel`
    // is the idempotent-release no-op on the common path (we never acquired) and the
    // genuine release on the just-healed-tripwire path (the heal re-acquired).
    if bkl_optout {
        akuma_exec::bkl::dropped_window_open();
    }
    // Record that this core serviced a user (EL0) trap — the M3 cross-core-userspace
    // proof (an EL0 trap only comes from userspace running on this core).
    #[cfg(kernel_smp_shared)]
    crate::smp_shared::record_el0_trap();
    let ret = rust_sync_el0_handler_inner(frame, esr, far);
    // Deferred thread-kill (real shared-kernel SMP): if a peer core's
    // kill_thread_group posted a kill request for this thread, terminate it
    // HERE — at the EL1→EL0 boundary, after the unwound syscall/fault call
    // stack has released every kernel lock — rather than where it was
    // preempted mid-critical-section (which would leak the locks: the sshd
    // "freeze"). The thread still holds the BKL at this point (same state as
    // the sys_exit/sys_exit_group self-termination paths); mark + yield
    // reconciles the BKL on switch-out. (Never reached for the exit paths,
    // which don't return from `_inner`.) An opted-out excursion first closes
    // its window and takes the lock — it never returns to EL0, so this is the
    // window's real end, and the terminal mark+yield must run in the same
    // BKL-held state every other path gives it.
    #[cfg(kernel_smp_shared)]
    if akuma_exec::threading::take_thread_kill_request() {
        if bkl_optout {
            akuma_exec::bkl::dropped_window_close_no_reacquire();
            akuma_exec::bkl::enter_kernel();
        }
        let tid = akuma_exec::threading::current_thread_id();
        akuma_exec::threading::mark_thread_terminated(tid);
        loop { akuma_exec::threading::yield_now(); }
    }
    if bkl_optout {
        akuma_exec::bkl::dropped_window_close_no_reacquire();
    } else {
        akuma_exec::bkl::leave_kernel();
    }
    // Tripwire for the SMP=4 mixed-EL corruption (BKL_RUSTC_SCALING_BASELINE.md §5.1):
    // after this returns, the asm epilogue erets to EL0 straight from this frame with
    // no further checks — this is one of the two eret paths the existing POISON
    // tripwires do not cover. An ELR in kernel text here means the frame was clobbered
    // during the excursion; catch it with ids before the eret makes it a userspace
    // SIGSEGV with a kernel register file.
    {
        let f = unsafe { &*frame };
        if akuma_exec::mmu::is_kernel_text(f.elr_el1 as usize) {
            crate::safe_print!(224,
                "[SVC POISON] eret->EL0 elr={:#x} spsr={:#x} sp_el0={:#x} x0={:#x} x1={:#x} x3={:#x} x8={:#x} tid={} core={}\n",
                f.elr_el1, f.spsr_el1, f.sp_el0, f.x0, f.x1, f.x3, f.x8,
                akuma_exec::threading::current_thread_id(),
                akuma_exec::bkl::current_core_id());
        }
    }
    // Last thing before the asm epilogue erets: this closes the window and, when
    // it is full, prints it — outside the VFS BKL and outside every span above.
    rp_span.end_exception();
    ret
}

/// Total stale dropped-window depths healed at EL0 entry (the tripwire in
/// [`rust_sync_el0_handler`]). Healthy runs keep this at 0 — a nonzero count means a
/// window leaked past an excursion's end (abnormal unwind skipping a guard destructor,
/// or a recycled thread slot inheriting depth) and is part of every phase's pass
/// criterion; the boot suite asserts it stays 0 (see `process_tests`).
#[cfg(kernel_smp_shared)]
static STALE_WINDOW_HEALS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Read the stale-window heal counter (see [`STALE_WINDOW_HEALS`]).
#[cfg(kernel_smp_shared)]
#[cfg_attr(not(kernel_tests), allow(dead_code))]
pub fn stale_window_heal_count() -> u64 {
    STALE_WINDOW_HEALS.load(core::sync::atomic::Ordering::Relaxed)
}

fn rust_sync_el0_handler_inner(frame: *mut UserTrapFrame, esr: u64, far: u64) -> u64 {
    let ec = (esr >> 26) & 0x3F; // Exception Class
    let iss = esr & 0x1FFFFFF; // Instruction Specific Syndrome

    match ec {
        esr::EC_SVC64 => {
            // System call - number in x8, args in x0-x5
            let frame_ref = unsafe { &*frame };
            let syscall_num = frame_ref.x8;
            // BKL-hold profiler: tag this core's excursion with the syscall number so a
            // waiting peer can attribute its BKL wait to this syscall (shared-kernel SMP).
            #[cfg(kernel_smp_shared)]
            akuma_exec::sync::set_holder_tag(akuma_exec::bkl::current_core_id(), syscall_num);

            // Stale-I-cache spurious-SVC guard (§7k.4 root cause). At an EC_SVC64
            // trap the hardware set ELR_EL1 to the instruction AFTER the svc, so
            // the instruction at ELR-4 MUST be an svc. User-memory reads are
            // cache-coherent, so if ELR-4 reads back as NOT an svc, the CPU
            // executed a stale-I-cache svc at a PC whose backing memory is no
            // longer an svc — a spurious syscall that would return an errno into
            // x0 and clobber the live pointer the real instruction expected
            // (the intermittent rustc WILD-DA: wait4(95)/futex with pointer args
            // → EFAULT/ENOSYS → `str [x0]` faults). Recover by flushing the
            // I-cache and re-executing from ELR-4 WITHOUT dispatching, so x0 is
            // never corrupted. This generalizes the >500 JIT workaround below
            // (which a VALID syscall number like 95 slips past). A legitimate
            // syscall always reads back an svc here, so there are no false
            // positives; the same-ELR replay counter bounds any non-I-cache
            // cause. A trap the replays don't heal is still NOT a syscall — it
            // is never dispatched (see `spurious_svc_give_up` below); the QEMU
            // DC-ZVA/STP-XZR misroute emulations further down get a chance to
            // claim it first.
            let mut spurious_svc_give_up = false;
            if crate::config::VERIFY_SVC_AT_ENTRY {
                use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
                static LAST_SPURIOUS_ELR: AtomicU64 = AtomicU64::new(0);
                static SPURIOUS_REPLAYS: AtomicU32 = AtomicU32::new(0);
                let elr = frame_ref.elr_el1;
                let prev_instr = elr.checked_sub(4).and_then(|prev_va| { read_user_instr(prev_va) });
                // Only act when the prev instruction is readable AND definitively not an svc.
                if let Some(instr) = prev_instr
                    && !is_aarch64_svc(instr)
                {
                    SPURIOUS_SVC_COUNT.fetch_add(1, Ordering::Relaxed);
                    let replays = if LAST_SPURIOUS_ELR.swap(elr, Ordering::Relaxed) == elr {
                        SPURIOUS_REPLAYS.fetch_add(1, Ordering::Relaxed) + 1
                    } else {
                        SPURIOUS_REPLAYS.store(1, Ordering::Relaxed);
                        1
                    };
                    if replays <= 8 {
                        crate::safe_print!(192,
                            "[SPURIOUS-SVC] stale-icache: nr={} elr={:#x} insn@elr-4={:#010x} (not svc) x0={:#x} — IC flush + replay #{}\n",
                            syscall_num, elr, instr, frame_ref.x0, replays);
                        unsafe {
                            core::arch::asm!("ic iallu");
                            core::arch::asm!("dsb ish");
                            core::arch::asm!("isb");
                            (*frame).elr_el1 = elr.wrapping_sub(4);
                        }
                        // Do NOT dispatch: return the live x0 so the epilogue restores
                        // the original (un-clobbered) register and re-runs the real insn.
                        return frame_ref.x0;
                    }
                    // Too many replays at the same ELR — unlikely to be I-cache.
                    // Still not a real syscall (insn@ELR-4 is not an svc), so it
                    // must NEVER be dispatched: writing a syscall return into x0
                    // clobbers a live user register (the SMP Go-heap corruption
                    // amplifier — memclr/spanSet.push pointers turning into
                    // errnos/timespecs → WILD-DA). Flag it; after the QEMU
                    // misroute emulations below decline it, deliver a signal.
                    crate::safe_print!(160,
                        "[SPURIOUS-SVC] giving up at elr={:#x} after {} replays — nr={} will NOT be dispatched\n",
                        elr, replays, syscall_num);
                    spurious_svc_give_up = true;
                }
            }

            // JIT cache coherency workaround: bogus syscall numbers (> 500)
            // indicate stale instruction cache — JIT wrote new code but the
            // CPU (or QEMU's TB cache) still has old translations.
            // IC IALLU from EL1 flushes the entire I-cache; on QEMU TCG this
            // calls tb_flush() which clears all translated blocks.
            // Counter-based: allow up to 16 consecutive retries before giving up.
            {
                use core::sync::atomic::{AtomicU32, Ordering};
                static JIT_RETRY_COUNT: AtomicU32 = AtomicU32::new(0);
                if syscall_num > 500 {
                    let count = JIT_RETRY_COUNT.fetch_add(1, Ordering::Relaxed);
                    if count < 16 {
                        let elr = frame_ref.elr_el1;
                        // AArch64 SVC encoding: bits[31:24]=0xD4, bits[23:21]=0b000,
                        // bits[4:0]=0b00001.  Mask: 0xFFE0001F == 0xD4000001.
                        // If the instruction at ELR-4 is itself a SVC, we MUST NOT
                        // back up ELR: the registers at IC flush entry are for the
                        // IC flush trampoline, not for the preceding syscall.
                        // Re-executing that SVC with wrong registers causes spurious
                        // syscalls (e.g. io_setup with ctx_idp=0x1 → EFAULT → WILD-DA).
                        // In that case just flush the IC and return to ELR (skip replay).
                        let prev_instr = elr.checked_sub(4).and_then(|prev_va| { read_user_instr(prev_va) });
                        let prev_is_svc = prev_instr.is_some_and(is_aarch64_svc);
                        // FIX_MEMORY_MAPPING.md / EPOLL_PERFORMANCE.md: stale icache can make the
                        // CPU decode the wrong SVC immediate; replay preserves ELR unless prev was SVC.
                        crate::safe_print!(128,
                            "[JIT] IC flush + replay #{} bogus nr={} ELR={:#x} prev={}\n",
                            count + 1, syscall_num, elr,
                            if prev_is_svc { "SVC(skip)" } else { "replay" });
                        unsafe {
                            core::arch::asm!("ic iallu");
                            core::arch::asm!("dsb ish");
                            core::arch::asm!("isb");
                            if !prev_is_svc {
                                (*frame).elr_el1 = elr.wrapping_sub(4);
                            }
                            // If prev_is_svc: ELR stays at the IC flush SVC itself.
                            // QEMU will retranslate from that address with the cleared TB.
                        }
                        // Check for pending signals before replaying — without this,
                        // SIGURG preemption is delayed until the next normal syscall,
                        // adding up to 10ms latency to Go's goroutine preemption.
                        //
                        // IMPORTANT: Only deliver async signals here (SIGURG=23 and similar).
                        // Fault signals (SIGSEGV=11, SIGBUS=7, SIGFPE=8, SIGILL=4) carry
                        // specific si_addr from the original fault.  Delivering them in the
                        // IC flush path gives the wrong fault_pc/si_addr context, causing
                        // Go's sigpanic handler to try patching code at the wrong address,
                        // which itself faults → re-entrant SIGSEGV → process killed.
                        const FAULT_SIGNALS: u64 = (1 << 4) | (1 << 7) | (1 << 8) | (1 << 11);
                        let sig_mask = akuma_exec::threading::thread_signal_mask();
                        // Block fault signals in this path by adding them to the effective mask.
                        let effective_mask = sig_mask | FAULT_SIGNALS;
                        if let Some(sig) = akuma_exec::threading::take_pending_signal(effective_mask) {
                            // For async signals like SIGURG, check if sigaltstack is ready
                            let thread_slot = akuma_exec::threading::current_thread_id();
                            let (alt_sp, _, _) = akuma_exec::threading::get_sigaltstack(thread_slot);
                            if sig == 23 && alt_sp == 0 {
                                crate::tprint!(96, "[SIGURG] re-pend tid={} (alt_sp=0, JIT retry)\n", thread_slot);
                                akuma_exec::threading::pend_signal_for_thread(thread_slot, sig);
                            } else {
                                unsafe { (*frame).x0 = frame_ref.x0; }
                                if try_deliver_signal(frame, sig, 0, false, esr) {
                                    return u64::from(sig);
                                }
                                apply_default_signal_action(sig); // diverges if fatal
                            }
                        }
                        // handle_syscall was not invoked; do not clear CURRENT_SYSCALL_NR /
                        // proc.current_syscall here — those are global/per-process and another
                        // thread may be inside handle_syscall (crash7 WILD-DA last_sc=!0 is OK).
                        return frame_ref.x0;
                    }
                    crate::safe_print!(128, "[JIT] giving up after {} retries, nr={}\n",
                        count + 1, syscall_num);
                    JIT_RETRY_COUNT.store(0, Ordering::Relaxed);
                } else {
                    JIT_RETRY_COUNT.store(0, Ordering::Relaxed);
                }
            }

            // QEMU-DC-ZVA-MISROUTING: QEMU TCG sometimes generates EC=0x15 (SVC)
            // instead of EC=0x18 (system instruction trap) for DC ZVA from EL0,
            // with ELR pointing at the DC ZVA instruction itself rather than SVC+4.
            // Detection: instruction AT ELR is DC ZVA (0xD50B74XX) AND instruction
            // at ELR-4 is NOT an SVC.  If both hold, emulate the DC ZVA, advance
            // ELR by 4, and return the goroutine's original x0 unchanged so that
            // x0 (the zero-target address) is never overwritten with a syscall
            // return value — which would crash the goroutine when it resumes DC ZVA.
            {
                let elr = frame_ref.elr_el1;
                let elr_instr = { read_user_instr(elr) };
                if elr_instr.is_some_and(|i| (i & !0x1F) == 0xD50B7420) {
                    let prev_instr = elr.checked_sub(4).and_then(|prev| { read_user_instr(prev) });
                    let prev_is_svc = prev_instr
                        .is_some_and(|i| (i & 0xFFE0001F) == 0xD4000001);
                    if !prev_is_svc {
                        // Misrouted DC ZVA: decode Xt register and emulate.
                        let xt = (elr_instr.unwrap_or(0) & 0x1F) as usize;
                        let dc_addr = if xt < 31 {
                            unsafe { core::ptr::read_volatile((frame as *const u64).add(xt)) }
                        } else { 0 };
                        emulate_dc_zva(dc_addr);
                        crate::syscall::syscall_counters::inc_qemu_dc_zva_ec15();
                        unsafe { (*frame).elr_el1 = elr.wrapping_add(4); }
                        return unsafe { (*frame).x0 };
                    }
                }
            }

            // QEMU-STP-XZR-MISROUTING: QEMU TCG sometimes generates EC=0x15 (SVC)
            // instead of EC=0x25 (data abort) for `stp xzr, xzr, [Xn, #N]` when Xn
            // points into a PROT_NONE lazy region (Go's sysReserve arena). Same class
            // as the DC ZVA misrouting above but for a different instruction.
            // Pattern 4 in GO_FORKTEST_DEBUG.md (crush, crash36.log).
            {
                let elr = frame_ref.elr_el1;
                let elr_instr = { read_user_instr(elr) };
                if let Some((rn, offset)) = elr_instr.and_then(decode_stp_xzr_xzr) {
                    let prev_instr = elr.checked_sub(4).and_then(|prev| { read_user_instr(prev) });
                    let prev_is_svc = prev_instr
                        .is_some_and(|i| (i & 0xFFE0001F) == 0xD4000001);
                    if !prev_is_svc {
                        // Misrouted trap with a garbage x8 that may have latched a BKL
                        // opt-out: the demand-page install below (map_user_page +
                        // frame tracking, no `as_lock`) ran BKL-held before Phase 7f —
                        // pause the window so it stays that way. Reopens on the early
                        // return (outer close stays balanced); no-op when no window.
                        let _held = akuma_exec::bkl::DroppedWindowPause::new();
                        let base = if rn < 31 {
                            unsafe { core::ptr::read_volatile((frame as *const u64).add(rn)) }
                        } else { 0 };
                        let store_va = (base as i64).wrapping_add(offset) as u64;
                        let page_va = (store_va as usize) & !0xFFF;
                        // Demand-page if target is in a PROT_NONE lazy region.
                        let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
                        let as_owner = akuma_exec::process::address_space_owner_pid_for_fault().unwrap_or(pid);
                        if akuma_exec::process::lazy_region_lookup_for_page_fault(pid, store_va as usize).is_some()
                            && let Some(pf) = crate::pmm::alloc_page_zeroed() {
                                let (tfs, installed) = unsafe {
                                    akuma_exec::mmu::map_user_page(page_va, pf.addr, akuma_exec::mmu::user_flags::RW_NO_EXEC)
                                };
                                if installed {
                                    if let Some(owner) = akuma_exec::process::lookup_process_shared(as_owner) {
                                        owner.address_space.track_user_frame(pf);
                                        for tf in tfs { owner.address_space.track_page_table_frame(tf); }
                                    } else {
                                        crate::pmm::free_page(pf);
                                        for tf in tfs { crate::pmm::free_page(tf); }
                                    }
                                } else {
                                    crate::pmm::free_page(pf);
                                    if let Some(owner) = akuma_exec::process::lookup_process_shared(as_owner) {
                                        for tf in tfs { owner.address_space.track_page_table_frame(tf); }
                                    } else {
                                        for tf in tfs { crate::pmm::free_page(tf); }
                                    }
                                }
                            }
                        emulate_stp_xzr_xzr(store_va);
                        crate::syscall::syscall_counters::inc_qemu_stp_xzr_ec15();
                        unsafe { (*frame).elr_el1 = elr.wrapping_add(4); }
                        return unsafe { (*frame).x0 };
                    }
                }
            }

            // Phantom SVC the replay guard gave up on and no QEMU misroute
            // emulation above claimed: this trap did not come from an `svc`, so
            // there is nothing to dispatch (a dispatch would corrupt x0 of a
            // thread that never made a syscall — and nr could even be 139,
            // making the sigreturn below restore garbage). Deliver SIGILL at
            // the trap PC; with no handler, kill the thread group like any
            // other fatal fault.
            if spurious_svc_give_up {
                // The garbage x8 may have latched a BKL opt-out for this excursion —
                // the signal/kill fallout below is lifecycle work and must run
                // BKL-held. Pause the window (no-op when none is open): the deliver
                // branch reopens it on return (the outer close stays balanced); the
                // kill branch never returns and `return_to_kernel` resets the ledger.
                let _held = akuma_exec::bkl::DroppedWindowPause::new();
                let elr = frame_ref.elr_el1;
                crate::safe_print!(128,
                    "[SPURIOUS-SVC] undispatchable phantom SVC at elr={:#x} — delivering SIGILL\n", elr);
                if try_deliver_signal(frame, 4, elr, true, esr) {
                    return 4; // signal number in x0 for the handler
                }
                if let Some(proc) = akuma_exec::process::current_process_shared() {
                    crate::safe_print!(128,
                        "[SPURIOUS-SVC] Process {} ({}) killed (SIGILL, phantom SVC)\n",
                        proc.pid, proc.name);
                }
                fatal_signal_group_exit(-4) // never returns
            }

            // rt_sigreturn (NR 139): restore saved context from signal frame
            if syscall_num == 139 {
                if let Some(saved_x0) = do_rt_sigreturn(frame) {
                    // Linux delivers pending signals on every return to user mode,
                    // including after rt_sigreturn. Without this check, a SIGURG
                    // arriving between a syscall return and rt_sigreturn can corrupt
                    // the next syscall's x0 (e.g. futex sees uaddr=1 instead of
                    // the real address). do_rt_sigreturn has already restored the
                    // full register set in *frame, so delivery here sees the correct
                    // SP/PC. We must set frame.x0 = saved_x0 before delivering so
                    // that sigreturn from the nested handler restores the right value.
                    let sig_mask = akuma_exec::threading::thread_signal_mask();
                    if let Some(sig) = akuma_exec::threading::take_pending_signal(sig_mask) {
                        // For async signals like SIGURG, check if sigaltstack is ready
                        let thread_slot = akuma_exec::threading::current_thread_id();
                        let (alt_sp, _, _) = akuma_exec::threading::get_sigaltstack(thread_slot);
                        if sig == 23 && alt_sp == 0 {
                            crate::tprint!(96, "[SIGURG] re-pend tid={} (alt_sp=0, sigreturn)\n", thread_slot);
                            akuma_exec::threading::pend_signal_for_thread(thread_slot, sig);
                        } else {
                            unsafe { (*frame).x0 = saved_x0; }
                            if try_deliver_signal(frame, sig, 0, false, esr) {
                                return u64::from(sig);
                            }
                            apply_default_signal_action(sig); // diverges if fatal
                        }
                    }
                    return saved_x0;
                }
                fatal_signal_group_exit(-11);
            }

            // Save trap frame pointer so fork/clone can read full register state
            akuma_exec::threading::set_current_trap_frame(frame.cast_const());
            let args = [
                frame_ref.x0,
                frame_ref.x1,
                frame_ref.x2,
                frame_ref.x3,
                frame_ref.x4,
                frame_ref.x5,
            ];

            // Handle syscall
            let ret = crate::syscall::handle_syscall(syscall_num, &args);

            // SYNC TLS: If the syscall modified TPIDR_EL0 (e.g. SET_TPIDR_EL0),
            // update the trap frame so the change persists after register restoration.
            unsafe {
                let current_tls: u64;
                core::arch::asm!("mrs {}, tpidr_el0", out(reg) current_tls);
                (*frame).tpidr_el0 = current_tls;
            }
            
            // Check if process exited - if so, return to kernel
            if let Some(proc) = akuma_exec::process::current_process_shared() {
                if proc.exited {
                    let exit_code = proc.exit_code;
                    
                    // Validate exit code - detect corruption (pointer-like values)
                    let exit_code_u32 = exit_code as u32;
                    if (0x40000000..0x50000000).contains(&exit_code_u32) {
                        safe_print!(128, "[exception] CORRUPT EXIT CODE DETECTED!\n");
                        crate::safe_print!(128, "  PID={}, exit_code={} (0x{:x}) looks like kernel address\n",
                            proc.pid, exit_code, exit_code_u32);
                        crate::safe_print!(96, "  proc ptr=0x{:x}, &exit_code=0x{:x}\n",
                            core::ptr::from_ref(proc) as usize, 
                            &raw const proc.exit_code as usize);
                        // Also check if the syscall frame x0 matches
                        let frame_x0 = unsafe { (*frame).x0 };
                        crate::safe_print!(64, "  frame.x0=0x{:x} (syscall arg)\n", frame_x0);
                    }
                    
                    let elapsed_us = (akuma_exec::runtime::runtime().uptime_us)()
                        .saturating_sub(proc.start_time_us);
                    let secs = elapsed_us / 1_000_000;
                    let frac = (elapsed_us % 1_000_000) / 10_000;
                    crate::safe_print!(128, "[exception] Process {} ({}) exited (code {}) [{}.{:02}s]\n",
                        proc.pid, proc.name, exit_code, secs, frac);
                    akuma_exec::process::return_to_kernel(exit_code);
                }
            } else {
                // Only log if we just handled EXIT syscall
                if syscall_num == 93 {
                    safe_print!(128, "[exception] WARNING: EXIT syscall but no current_process!\n");
                }
            }

            akuma_exec::threading::clear_current_trap_frame();

            // Deliver any pending signal (e.g. SIGURG for Go goroutine preemption).
            // sys_tkill pends the signal; we deliver it here so the target thread
            // sees it at the next syscall boundary (async delivery via pending queue).
            let sig_mask = akuma_exec::threading::thread_signal_mask();

            if crate::config::TRACE_TKILL {
                let slot = akuma_exec::threading::current_thread_id();
                let pend = akuma_exec::threading::pending_signals_raw(slot);
                if pend != 0 {
                    crate::safe_print!(128,
                        "[signal] syscall-ret nr={} slot={} pending={:#x} mask={:#x}\n",
                        syscall_num, slot, pend, sig_mask);
                }
            }

            if let Some(sig) = akuma_exec::threading::take_pending_signal(sig_mask) {
                // For async signals like SIGURG (23), check if sigaltstack is configured.
                // Go threads call sigaltstack during mstart1 - if not yet configured,
                // the thread isn't ready to handle signals. Re-pend and deliver later.
                let thread_slot = akuma_exec::threading::current_thread_id();
                let (alt_sp, _, _) = akuma_exec::threading::get_sigaltstack(thread_slot);
                if sig == 23 && alt_sp == 0 {
                    // Re-pend SIGURG for later - thread not ready yet
                    crate::tprint!(96, "[SIGURG] re-pend tid={} (alt_sp=0, syscall return)\n", thread_slot);
                    akuma_exec::threading::pend_signal_for_thread(thread_slot, sig);
                } else {
                    // Store the syscall return value in x0 of the trap frame so that
                    // sigreturn restores it correctly (the signal handler's x0 = sig,
                    // and after sigreturn the caller sees x0 = syscall result).
                    unsafe { (*frame).x0 = ret; }
                    if try_deliver_signal(frame, sig, 0, false, esr) {
                        return u64::from(sig); // x0 = signal number for the handler
                    }
                    // No user handler took it — apply the default action, which
                    // for a fatal signal (SIGABRT from abort(), SIGTERM from
                    // kill(2), …) kills the thread group and never returns.
                    apply_default_signal_action(sig);
                }
                // Delivery failed (re-pended / bad stack) or the default action is
                // to discard; just return normally.
            }

            ret
        }
        esr::EC_DATA_ABORT_LOWER => {
            // `far` is the entry snapshot (see rust_sync_el0_handler); ELR comes
            // from the trap frame — the live ELR_EL1 is consumed by any eret in
            // between (a preempting IRQ's return), so a late `mrs elr_el1` reads
            // a kernel resume address, not the faulting user PC.
            let elr = unsafe { (*frame).elr_el1 };

            let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
            // Thread-group leader: mmap_regions and fault_mutex live on the leader Process
            // (CLONE_VM worker threads have their own Process slot but empty mmap_regions).
            let as_owner = akuma_exec::process::address_space_owner_pid_for_fault().unwrap_or(pid);
            #[cfg(kernel_smp_shared)]
            akuma_exec::sync::set_holder_tag(akuma_exec::bkl::current_core_id(), akuma_exec::sync::HOLD_TAG_FAULT);

            // Translation/permission fault (ISS bits [5:2]) — try demand paging
            let fault_type = iss & 0x3C; // DFSC[5:2]
            let is_translation_fault = fault_type == 0x04 || fault_type == 0x08;
            let is_permission_fault = fault_type == 0x0C;
            let far_usize = far as usize;

            if is_permission_fault {
                let is_write = (iss & (1 << 6)) != 0; // ISS bit 6 = WnR
                // CoW write fault: write to a shared read-only page
                if is_write {
                    let page_va = far_usize & !(0xFFF);

                    // Did someone already fix this? A sibling thread on another core
                    // can break CoW on this page between the CPU taking the fault and
                    // this handler running, which leaves the write legal and every
                    // repair path below unable to say so — the CoW reference the
                    // winner consumed is gone, and an ELF `.data`/`.bss` page has no
                    // region record to fall back on. Ask the page table first.
                    if stale_write_fault_absorbed(page_va) {
                        return unsafe { (*frame).x0 };
                    }

                    // Serialize CoW fault handling per-page to prevent races
                    // when multiple CLONE_VM threads fault on the same page.
                    // Holder-tracked so a sibling can reclaim the slot if the
                    // holder died mid-fault (see fault_slot_acquire).
                    let _cow_fault_guard = fault_slot_hold(pid, as_owner, page_va);

                    // Allocate the CoW destination BEFORE taking `as_lock` — alloc must
                    // stay outside the hold (the PMM OOM/reclaim path can re-enter
                    // `as_lock`). Freed below if the page didn't actually need CoW.
                    if let Some(new_frame) = crate::pmm::alloc_page_zeroed() {
                        let mut did_cow = false;
                        if let Some(owner) = akuma_exec::process::lookup_process_shared(as_owner) {
                            // PTE overwrite + 4 KiB copy under `as_lock` (shared-kernel
                            // SMP): a concurrent munmap/fault on this AS is excluded, so
                            // `old_pa` stays valid across the copy. Short: no alloc/IO here.
                            #[cfg(kernel_smp_shared)]
                            let _asg = akuma_exec::process::AsLockHold::new(&owner.as_lock);
                            let ttbr0: u64;
                            unsafe { core::arch::asm!("mrs {}, TTBR0_EL1", out(reg) ttbr0); }
                            let l0_addr = (ttbr0 & 0x0000_FFFF_FFFF_F000) as usize;
                            let l0_ptr = akuma_exec::mmu::phys_to_virt(l0_addr) as *const u64;
                            if let Some(old_pa_with_offset) = akuma_exec::mmu::translate_user_va(l0_ptr, page_va) {
                                let old_pa = old_pa_with_offset & !(0xFFF);
                                if crate::pmm::cow_ref_get(old_pa) > 0 {
                                    // `_asg` above already holds `as_lock` for this whole
                                    // block, so the break must not take it again — and the
                                    // 4 KiB copy lands inside that hold, which is what keeps
                                    // `old_pa` valid across it (the invariant F1 wants on the
                                    // EL1 paths too).
                                    complete_cow_break(
                                        CowRemap::CallerHoldsAsLock(owner),
                                        page_va, old_pa, new_frame,
                                    );
                                    did_cow = true;
                                }
                            }
                        }
                        if did_cow {
                            return unsafe { (*frame).x0 };
                        }
                        // Not a CoW page (or no owner): return the unused frame.
                        crate::pmm::free_page(new_frame);
                    }
                    // OOM or not-CoW: fall through to the lazy permission upgrade / SIGSEGV.
                }

                // Lazy region permission upgrade (e.g. demand-paged RO → RW after mprotect)
                if let Some((region_flags, _source, _region_start, _region_size)) = akuma_exec::process::lazy_region_lookup_for_page_fault(pid, far_usize)
                    && !akuma_exec::mmu::user_flags::is_none(region_flags) {
                        let page_va = far_usize & !(0xFFF);
                        if let Some(owner) = akuma_exec::process::lookup_process_shared(as_owner) {
                            // PTE permission edit under `as_lock` (shared-kernel SMP); free
                            // fn resolves the current TTBR0, so a shared `&Process` suffices.
                            #[cfg(kernel_smp_shared)]
                            let _asg = akuma_exec::process::AsLockHold::new(&owner.as_lock);
                            #[cfg(not(kernel_smp_shared))]
                            let _ = &owner;
                            akuma_exec::mmu::update_current_user_page_flags(page_va, akuma_exec::mmu::user_flags::RW_NO_EXEC);
                            return unsafe { (*frame).x0 };
                        }
                    }

                // Eager region permission upgrade — the same repair, for the regions
                // that had no path to it. An eager `mmap` installs its pages up front
                // and registers no lazy region, so a page inside one that is somehow
                // read-only reached neither the CoW break (no `cow_ref`) nor the
                // upgrade above (no lazy region) and died with SIGSEGV, even though
                // the mapping is writable and the write is legitimate. That is the
                // `[WPF] ... cow_ref=0 lazy_self=NONE` signature that killed cargo
                // mid-build.
                //
                // Gated on the region actually being writable, so `mprotect(PROT_READ)`
                // and `PROT_NONE` guard pages still fault the way they must — which is
                // exactly what `MmapRegion::flags` was added to make knowable.
                if is_write
                    && let Some(region_flags) =
                        akuma_exec::process::eager_region_flags_for_page_fault(pid, far_usize)
                    && region_flags & akuma_exec::mmu::flags::AP_MASK
                        == akuma_exec::mmu::flags::AP_RW_ALL
                {
                    let page_va = far_usize & !(0xFFF);
                    // "The PTE already grants this write" is handled at the top of the
                    // write arm now (`stale_write_fault_absorbed`), for every mapping
                    // rather than only the ones with an eager region — an ELF
                    // `.data`/`.bss` page has no region at all and was dying here.
                    // Reaching this point therefore means the PTE really is read-only
                    // (or the retry bound gave up), so a permission rewrite is the
                    // right repair, not an invalidation.
                    if let Some(owner) = akuma_exec::process::lookup_process_shared(as_owner) {
                        #[cfg(kernel_smp_shared)]
                        let _asg = akuma_exec::process::AsLockHold::new(&owner.as_lock);
                        #[cfg(not(kernel_smp_shared))]
                        let _ = &owner;
                        // Probe BEFORE the repair: once the PTE is RW the anomalous
                        // state is gone, and it is the state — not the repair — that
                        // the null-`Rc` hunt needs on the record. In run 3 of the
                        // self-host build this arm fired on cargo's heap page
                        // 0x314da000 six log lines before the process read a zeroed
                        // pointer off that same page, so whatever this prints is the
                        // closest look at the defect we have.
                        print_page_forensics("EAGER-UPGRADE", pid, page_va);
                        akuma_exec::mmu::update_current_user_page_flags(page_va, region_flags);
                        crate::safe_print!(192,
                            "[EAGER-UPGRADE] pid={} as_owner={} va={:#x} flags={:#x}\n",
                            pid, as_owner, page_va, region_flags);
                        return unsafe { (*frame).x0 };
                    }
                }
            }

            if is_translation_fault {
                let lazy_found = akuma_exec::process::lazy_region_lookup_for_page_fault(pid, far_usize);
                if lazy_found.is_none() {
                    let lr_count = akuma_exec::process::lazy_region_count_for_pid(pid);
                    // Also check the parent PID - maybe lazy regions weren't cloned
                    let parent_pid = akuma_exec::process::lookup_process_shared(as_owner)
                        .map_or(0, |p| p.parent_pid);
                    let parent_lr_count = akuma_exec::process::lazy_region_count_for_pid(parent_pid);
                    let parent_has_va = akuma_exec::process::lazy_region_lookup_for_pid(parent_pid, far_usize).is_some();
                    crate::safe_print!(256, "[DA-MISS] pid={} ppid={} va=0x{:x} lr_count={} parent_lr={} parent_has_va={}\n",
                        pid, parent_pid, far_usize, lr_count, parent_lr_count, parent_has_va);
                    akuma_exec::process::lazy_region_debug(far_usize);
                }
                if let Some((flags, source, region_start, region_size)) = lazy_found {
                    // A `PROT_NONE` region whose source is a FILE is not an anonymous
                    // reservation, and auto-committing it with a zeroed frame below
                    // would hand the process zeros where the file's bytes belong — no
                    // short read, no error, nothing to see anywhere downstream. Fall
                    // through to the normal file demand-paging path instead.
                    let protnone_file = akuma_exec::mmu::user_flags::is_none(flags)
                        && matches!(source, akuma_exec::process::LazySource::File { .. });
                    if protnone_file {
                        crate::pmm::dp_count(&crate::pmm::DP_PROTNONE_FILE_REGION, 1);
                        crate::tprint!(224,
                            "[DA-NONE-FILE] pid={} as_owner={} va={:#x} flags={:#x} — PROT_NONE flags on a FILE-backed lazy region, demand-paging instead of zero-filling\n",
                            pid, as_owner, far_usize, flags);
                    }
                    if akuma_exec::mmu::user_flags::is_none(flags) && !protnone_file {
                        // Auto-commit anonymous PROT_NONE reservation on first access.
                        // Go's sysReserve calls mmap(PROT_NONE) then sysMap calls
                        // mmap(PROT_RW, MAP_FIXED) to commit subranges. When the parent
                        // process accesses a reserved-but-uncommitted page we demand-page
                        // it as RW rather than SIGSEGVing. Guard pages are NOT lazy-region
                        // entries so this path is only reached for genuine reservations.
                        let page_va = far_usize & !(0xFFF);
                        if let Some(page_frame) = crate::pmm::alloc_page_zeroed() {
                            // Frame allocated OUTSIDE `as_lock`; install + track under it.
                            let mut installed_ok = false;
                            if let Some(owner) = akuma_exec::process::lookup_process_shared(as_owner) {
                                #[cfg(kernel_smp_shared)]
                                let _asg = akuma_exec::process::AsLockHold::new(&owner.as_lock);
                                let (table_frames, installed) = unsafe {
                                    akuma_exec::mmu::map_user_page(page_va, page_frame.addr, akuma_exec::mmu::user_flags::RW_NO_EXEC)
                                };
                                for tf in table_frames {
                                    owner.address_space.track_page_table_frame(tf);
                                }
                                if installed {
                                    owner.address_space.track_user_frame(page_frame);
                                    installed_ok = true;
                                }
                            }
                            if installed_ok {
                                crate::pmm::dp_count(&crate::pmm::DP_PROTNONE_PAGES, 1);
                                crate::syscall::syscall_counters::inc_pagefault(1);
                            } else {
                                // Race (another CPU mapped it) or no owner: free our page.
                                crate::pmm::free_page(page_frame);
                            }
                            return unsafe { (*frame).x0 };
                        }
                        // OOM: fall through to SIGSEGV
                        crate::tprint!(128, "[DA-NONE] pid={} as_owner={} far=0x{:x} OOM\n",
                            pid, as_owner, far_usize);
                    } else if far_in_kernel_identity_user_range(far) {
                        // Fault VA is in the kernel identity-map range — demand-paging here
                        // would corrupt kernel memory.  Fall through to SIGSEGV.
                        crate::tprint!(128, "[DA-DP] pid={} fault in kernel VA range {:#x} -> SIGSEGV\n",
                            pid, far_usize);
                    } else if demand_page_lazy_region(
                        akuma_exec::mmu::FaultAccess::Data,
                        pid, as_owner, far_usize, flags, &source, region_start, region_size,
                    ) {
                        return unsafe { (*frame).x0 };
                    }
                } else {
                    // Fallback: check eager mmap regions — the PTE may have been lost.
                    // Use lookup_process(as_owner): thread-group leader (tgid). current_process() goes
                    // through THREAD_PID_MAP and returns the *worker* thread's Process
                    // for CLONE_VM threads — that Process has empty mmap_regions because
                    // all mmaps were performed on the parent.
                    let page_va = far_usize & !0xFFF;
                    let mut recovered = false;
                    if let Some(owner) = akuma_exec::process::lookup_process_shared(as_owner) {
                        // Find the frame for this page under vm_lock (pure Vec read),
                        // then map it under `as_lock` (map allocs page tables and must not
                        // run while vm_lock is held; the two locks never nest here).
                        // `frame_for` needs a real PA, so it consults the owned
                        // frame list and returns None for a CoW-inherited region
                        // (extent known, no owned frames) — there is nothing to
                        // re-map from here in that case.
                        let phys_opt = owner.vm_with_regions(|r| {
                            r.iter().find_map(|reg| reg.frame_for(page_va))
                        });
                        if let Some(phys) = phys_opt {
                            crate::tprint!(192, "[DP-eager] pid={} re-map va=0x{:x} frame=0x{:x}\n",
                                pid, page_va, phys.addr);
                            #[cfg(kernel_smp_shared)]
                            let _asg = akuma_exec::process::AsLockHold::new(&owner.as_lock);
                            let (table_frames, _) = unsafe {
                                akuma_exec::mmu::map_user_page(page_va, phys.addr, akuma_exec::mmu::user_flags::RW_NO_EXEC)
                            };
                            for tf in table_frames {
                                owner.address_space.track_page_table_frame(tf);
                            }
                            recovered = true;
                        }
                    }
                    if recovered {
                        return unsafe { (*frame).x0 };
                    }
                    // Dump mmap_regions for debugging: shows what the eager fallback searched
                    if let Some(dbg_proc) = akuma_exec::process::lookup_process_shared(as_owner) {
                        let n = dbg_proc.vm_with_regions(|r| r.len());
                        crate::tprint!(128, "[DP] eager miss: pid={} va=0x{:x} checked {} mmap_regions\n",
                            pid, far_usize, n);
                    } else {
                        crate::tprint!(128, "[DP] eager miss: lookup_process({}) returned None!\n", pid);
                    }
                    let lazy_count = akuma_exec::process::lazy_region_count_for_pid(pid);
                    akuma_exec::process::lazy_region_debug(far_usize);
                    crate::tprint!(128, "[DP] no lazy region for FAR={:#x} pid={} (pid has {} lazy regions)\n", far, pid, lazy_count);
                    
                    // Log register state for debugging wild pointer accesses
                    let frame_ref = unsafe { &*frame };
                    let last_sc = crate::syscall::current_syscall_nr();
                    
                    // Check if FAR looks like a negative errno (syscall error used as pointer)
                    // Errno values are small negatives: -1 (EPERM) to -133 (EHWPOISON)
                    // As unsigned: 0xFFFFFFFFFFFFFFFF (-1) to 0xFFFFFFFFFFFFFF7B (-133)
                    let far_signed = far as i64;
                    if (-200..0).contains(&far_signed) {
                        let errno = -far_signed;
                        let errno_name = match errno {
                            1 => "EPERM", 2 => "ENOENT", 3 => "ESRCH", 4 => "EINTR",
                            9 => "EBADF", 11 => "EAGAIN", 12 => "ENOMEM", 13 => "EACCES",
                            14 => "EFAULT", 17 => "EEXIST", 19 => "ENODEV", 20 => "ENOTDIR",
                            96 => "EPFNOSUPPORT",
                            21 => "EISDIR", 22 => "EINVAL", 28 => "ENOSPC", 38 => "ENOSYS",
                            95 => "ENOTSUP", 97 => "EAFNOSUPPORT", 110 => "ETIMEDOUT",
                            115 => "EINPROGRESS",
                            _ => "???",
                        };
                        crate::tprint!(256, "[WILD-DA] *** FAR={:#x} is -{} ({}) - syscall error used as pointer! ***\n",
                            far, errno, errno_name);
                        crate::tprint!(128, "[WILD-DA] This means a syscall returned error -{} and userspace used it as a pointer\n", errno);
                        // §7k investigation (signal/register corruption, docs/SIGNAL_DELIVERY_FORKTEST_EVIDENCE.md):
                        // dump the user instruction at the FAULT and the source syscall site so a
                        // recurrence tells us whether the errno came from a REAL svc (genuine arg/
                        // register corruption, e.g. via signal sigframe restore) or an instruction
                        // mis-decode. We disassemble the faulting instr (to find which reg holds the
                        // errno) and check whether the byte stream looks like a corrupted load/store.
                        {
                            let elr = frame_ref.elr_el1;
                            let mut ib = [0u8; 8];
                            let ok = akuma_exec::mmu::user_access::copy_from_user_with(
                                &mut ib,
                                elr.wrapping_sub(4),
                                akuma_exec::mmu::user_access::Prefault::No,
                            )
                            .is_ok();
                            if ok {
                                let prev = u32::from_le_bytes([ib[0], ib[1], ib[2], ib[3]]);
                                let at = u32::from_le_bytes([ib[4], ib[5], ib[6], ib[7]]);
                                crate::safe_print!(200,
                                    "[WILD-DA-diag] elr={:#x} insn@elr={:#010x} insn@elr-4={:#010x}{} x8={:#x}\n",
                                    elr, at, prev,
                                    if (prev & 0xFFE0_001F) == 0xD400_0001 { " (PREV-IS-SVC!)" } else { "" },
                                    frame_ref.x8);
                            }
                        }
                    }
                    
                    crate::tprint!(384, "[WILD-DA] pid={} FAR={:#x} ELR={:#x} last_sc={}\n  x0={:#x} x1={:#x} x2={:#x} x3={:#x}\n  x4={:#x} x5={:#x} x6={:#x} x7={:#x}\n",
                        pid, far_usize, frame_ref.elr_el1, last_sc,
                        frame_ref.x0, frame_ref.x1, frame_ref.x2, frame_ref.x3,
                        frame_ref.x4, frame_ref.x5, frame_ref.x6, frame_ref.x7);
                    crate::tprint!(128, "  x8={:#x} x9={:#x} x10={:#x} x11={:#x}\n",
                        frame_ref.x8, frame_ref.x9, frame_ref.x10, frame_ref.x11);
                    crate::tprint!(128, "  x12={:#x} x13={:#x} x14={:#x} x15={:#x}\n",
                        frame_ref.x12, frame_ref.x13, frame_ref.x14, frame_ref.x15);
                    crate::tprint!(128, "  x16={:#x} x17={:#x} x18={:#x} x28={:#x}\n",
                        frame_ref.x16, frame_ref.x17, frame_ref.x18, frame_ref.x28);

                    // A null FAR means the *pointer* was corrupt, not the access —
                    // the interesting page is the one the null was loaded FROM, not
                    // FAR itself. In the run-3 autopsy that was `x0`
                    // (`ldr x8,[x0,#288]` with x0 in cargo's heap), so probe the page
                    // behind the argument registers most likely to hold that base.
                    // Frames on the PMM free list here are the null-`Rc` defect
                    // caught in the act (docs/archive/CARGO_HEAP_NULL_RC.md).
                    // Before anything else: is the faulting value the PMM's own
                    // quarantine poison? `poison_word` XORs the magic with the
                    // frame's PA, so a poisoned pointer carries the identity of the
                    // frame it came from — and decoding it converts "a qword read
                    // back as garbage" into "frame P, freed by thread T". That is
                    // what cracked the 2026-08-12 crash (`FAR=0xfeedfacea8d0e010`
                    // → `poison_word(0x767de000)` + 0x10), and the kernel should not
                    // need a human with a calculator to say so
                    // (docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md §13.8).
                    //
                    // Every register the instruction could have used as a base, not
                    // just FAR: the fault address is base+offset, so the undisplaced
                    // pointer is the one that decodes cleanly.
                    for (name, val) in [("FAR", far_usize as u64), ("x0", frame_ref.x0),
                                        ("x1", frame_ref.x1), ("x2", frame_ref.x2),
                                        ("x3", frame_ref.x3), ("x8", frame_ref.x8),
                                        ("x19", frame_ref.x19), ("x20", frame_ref.x20)] {
                        crate::pmm::report_poison_value(name, val);
                    }

                    for (name, val) in [("x0", frame_ref.x0), ("x1", frame_ref.x1),
                                        ("x19", frame_ref.x19), ("FAR", far_usize as u64)] {
                        // Skip obvious non-pointers: page 0 and kernel-range values.
                        if val >= 0x1000 && (val as usize) < akuma_exec::process::types::ProcessMemory::KERNEL_VA_START {
                            print_page_forensics(name, pid, val as usize);
                        }
                    }

                    // Auto-dump syscall log for post-crash diagnosis.
                    // Note: CLONE_VM threads share the address space owner's process info
                    // page, so read_current_pid() returns the owner PID for all siblings —
                    // the syscall log is stored under that owner PID, not the thread's own PID.
                    // Box 0: this is the kernel's own crash dump to the console, not a
                    // procfs read on behalf of a container, so it sees every log.
                    match crate::syscall::log::get_formatted(pid, 0) {
                        Some(log_bytes) => {
                            crate::safe_print!(64, "[WILD-DA] syscall log (pid={}):\n", pid);
                            if let Ok(s) = core::str::from_utf8(&log_bytes) {
                                for line in s.lines() {
                                    crate::safe_print!(128, "  {}\n", line);
                                }
                            }
                        }
                        None => {
                            crate::safe_print!(128, "[WILD-DA] no syscall log for pid={} (CLONE_VM thread? check owner PID)\n", pid);
                        }
                    }
                }
            }

            // Try delivering SIGSEGV to a registered userspace handler
            // Log all SIGSEGV deliveries to understand crashes
            {
                let far_usize = far as usize;
                if (0x0001_e000_0000..0x0002_0000_0000).contains(&far_usize) {
                    // Fault in Go heap range - always log for debugging
                    crate::tprint!(128, "[SIGSEGV-HEAP] pid={} far={:#x} elr={:#x} iss={:#x}\n",
                        pid, far, elr, iss);
                }
            }
            {
                let fr = unsafe { &*frame };
                maybe_print_sigsegv_syscall_diag(elr, far, fr);
            }
            if try_deliver_signal(frame, 11, far, true, esr) {
                return 11; // signal number in x0 for the handler
            }

            let frame_ref = unsafe { &*frame };
            crate::tprint!(128, "[Fault] Data abort from EL0 at FAR={:#x}, ELR={:#x}, ISS={:#x}\n",
                far, elr, iss);
            crate::safe_print!(128, "[Fault]  x0={:#x} x1={:#x} x2={:#x} x3={:#x}\n",
                frame_ref.x0, frame_ref.x1, frame_ref.x2, frame_ref.x3);
            crate::safe_print!(128, "[Fault]  x19={:#x} x20={:#x} x29={:#x} x30={:#x}\n",
                frame_ref.x19, frame_ref.x20, frame_ref.x29, frame_ref.x30);
            crate::safe_print!(128, "[Fault]  SP_EL0={:#x} SPSR={:#x} TPIDR_EL0={:#x}\n",
                frame_ref.sp_el0, frame_ref.spsr_el1, frame_ref.tpidr_el0);
            print_spawn_fault_diag(far, frame_ref);
            print_write_perm_fault_diag(far, iss, pid, as_owner);
            if let Some(proc) = akuma_exec::process::current_process_shared() {
                let elapsed_us = (akuma_exec::runtime::runtime().uptime_us)()
                    .saturating_sub(proc.start_time_us);
                let secs = elapsed_us / 1_000_000;
                let frac = (elapsed_us % 1_000_000) / 10_000;
                crate::safe_print!(128, "[Fault] Process {} ({}) SIGSEGV after {}.{:02}s\n",
                    proc.pid, proc.name, secs, frac);
            }
            fatal_signal_group_exit(-11) // SIGSEGV - never returns
        }
        esr::EC_INST_ABORT_LOWER => {
            // `far` is the entry snapshot (see rust_sync_el0_handler).
            let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
            let as_owner = akuma_exec::process::address_space_owner_pid_for_fault().unwrap_or(pid);
            #[cfg(kernel_smp_shared)]
            akuma_exec::sync::set_holder_tag(akuma_exec::bkl::current_core_id(), akuma_exec::sync::HOLD_TAG_FAULT);

            let fault_type = iss & 0x3C;
            let is_translation_fault = fault_type == 0x04 || fault_type == 0x08;
            let is_permission_fault = fault_type == 0x0C;
            let far_usize = far as usize;

            // Lazy region permission upgrade: an instruction fetch hit a page this
            // address space has mapped non-executable. Upgrade it to `RX` and run the
            // I-cache maintenance for the frame, which the demand-paging body
            // deliberately skipped for a non-exec mapping (COW_PILE_AUDIT.md §12.1) —
            // `invalidate_icache_for_page_va` is the full `dc cvau`/`ic ivau`/`dsb
            // ish`/`isb` sequence over the page, so this is the *only* site that has
            // to pay for it, and it pays once per page rather than once per mapper.
            //
            // **This does not check that `region_flags` is executable** — any lazy
            // region that is not `PROT_NONE` gets promoted to `RX`, so jumping into a
            // `PROT_READ|PROT_WRITE` file mapping silently makes it executable. W^X is
            // not enforced on this path. Recorded as F9 in COW_PILE_AUDIT.md §9 and
            // deliberately left alone here: refusing the promotion is a user-visible
            // behaviour change (a legitimate-looking fetch would start taking SIGSEGV)
            // and belongs in its own change with its own verification, not inside a
            // body merge.
            if is_permission_fault
                && let Some((region_flags, _source, _region_start, _region_size)) = akuma_exec::process::lazy_region_lookup_for_page_fault(pid, far_usize)
                    && !akuma_exec::mmu::user_flags::is_none(region_flags) {
                        let page_va = far_usize & !(0xFFF);
                        if let Some(owner) = akuma_exec::process::lookup_process_shared(as_owner) {
                            // One-shot, because this is the recovery path the merged
                            // body's `is_exec` gate relies on and nothing had ever
                            // observed it running. Printing per fault would be a
                            // per-page log line on the fault path; printing once names
                            // the class and its first instance.
                            if !IA_PERM_UPGRADE_SEEN
                                .swap(true, core::sync::atomic::Ordering::Relaxed)
                            {
                                crate::safe_print!(192,
                                    "[IA-PERM-UPGRADE] pid={} as_owner={} va={:#x} region_flags={:#x} -> RX (first of boot)\n",
                                    pid, as_owner, page_va, region_flags);
                            }
                            // PTE permission edit under `as_lock` (shared-kernel SMP).
                            #[cfg(kernel_smp_shared)]
                            let _asg = akuma_exec::process::AsLockHold::new(&owner.as_lock);
                            akuma_exec::mmu::update_current_user_page_flags(page_va, akuma_exec::mmu::user_flags::RX);
                            owner.address_space.invalidate_icache_for_page_va(page_va);
                            return unsafe { (*frame).x0 };
                        }
                    }

            if is_translation_fault {
                // #region debug lazy region miss
                let lazy_found = akuma_exec::process::lazy_region_lookup_for_page_fault(pid, far_usize);
                if lazy_found.is_none() {
                    let lr_count = akuma_exec::process::lazy_region_count_for_pid(pid);
                    let parent_pid = akuma_exec::process::lookup_process_shared(as_owner)
                        .map_or(0, |p| p.parent_pid);
                    let parent_lr_count = akuma_exec::process::lazy_region_count_for_pid(parent_pid);
                    let parent_has_va = akuma_exec::process::lazy_region_lookup_for_pid(parent_pid, far_usize).is_some();
                    crate::safe_print!(256, "[IA-MISS] pid={} ppid={} va=0x{:x} lr_count={} parent_lr={} parent_has_va={}\n",
                        pid, parent_pid, far_usize, lr_count, parent_lr_count, parent_has_va);
                    akuma_exec::process::lazy_region_debug(far_usize);
                }
                // #endregion
                if let Some((flags, source, region_start, region_size)) = lazy_found {
                    // A `PROT_NONE` region whose source is a FILE is not an anonymous
                    // reservation, and auto-committing it with a zeroed frame below
                    // would hand the process zeros where the file's bytes belong — no
                    // short read, no error, nothing to see anywhere downstream. Fall
                    // through to the normal file demand-paging path instead.
                    let protnone_file = akuma_exec::mmu::user_flags::is_none(flags)
                        && matches!(source, akuma_exec::process::LazySource::File { .. });
                    if protnone_file {
                        crate::pmm::dp_count(&crate::pmm::DP_PROTNONE_FILE_REGION, 1);
                        crate::tprint!(224,
                            "[DA-NONE-FILE] pid={} as_owner={} va={:#x} flags={:#x} — PROT_NONE flags on a FILE-backed lazy region, demand-paging instead of zero-filling\n",
                            pid, as_owner, far_usize, flags);
                    }
                    if akuma_exec::mmu::user_flags::is_none(flags) && !protnone_file {
                        // PROT_NONE: don't demand-page, fall through to SIGSEGV
                    } else if far_in_kernel_identity_user_range(far) {
                        // Fault VA is in the kernel identity-map range — demand-paging
                        // would corrupt kernel memory.  Fall through to SIGSEGV.
                        crate::tprint!(128, "[IA-DP] pid={} fault in kernel VA range {:#x} -> SIGSEGV\n",
                            pid, far_usize);
                    } else if demand_page_lazy_region(
                        akuma_exec::mmu::FaultAccess::Instruction,
                        pid, as_owner, far_usize, flags, &source, region_start, region_size,
                    ) {
                        return unsafe { (*frame).x0 };
                    }
                } else {
                    akuma_exec::process::lazy_region_debug(far_usize);
                    crate::tprint!(128, "[DP] no lazy region for inst FAR={:#x} pid={}\n", far, pid);
                    
                    // Log register state for debugging wild pointer accesses
                    let frame_ref = unsafe { &*frame };
                    crate::tprint!(256, "[WILD-IA] pid={} FAR={:#x} ELR={:#x} x0={:#x} x1={:#x} x2={:#x}\n",
                        pid, far_usize, frame_ref.elr_el1, frame_ref.x0, frame_ref.x1, frame_ref.x2);
                    crate::tprint!(128, "  x8={:#x} x9={:#x} x16={:#x} x17={:#x} x28={:#x}\n",
                        frame_ref.x8, frame_ref.x9, frame_ref.x16, frame_ref.x17, frame_ref.x28);
                }
            }

            // Try delivering SIGSEGV to a registered userspace handler
            {
                let fr = unsafe { &*frame };
                maybe_print_sigsegv_syscall_diag(fr.elr_el1, far, fr);
            }
            if try_deliver_signal(frame, 11, far, true, esr) {
                return 11;
            }

            crate::safe_print!(128, "[IA] pid={} far={:#x} iss={:#x}\n", pid, far, iss);
            interp_relr_forensics(far_usize, pid);
            let frame_ref = unsafe { &*frame };
            crate::tprint!(128, "[Fault] Instruction abort from EL0 at FAR={:#x}, ISS={:#x}\n",
                far, iss);
            crate::safe_print!(128, "[Fault]  x0={:#x} x1={:#x} x2={:#x} x3={:#x}\n",
                frame_ref.x0, frame_ref.x1, frame_ref.x2, frame_ref.x3);
            crate::safe_print!(128, "[Fault]  x19={:#x} x20={:#x} x29={:#x} x30={:#x}\n",
                frame_ref.x19, frame_ref.x20, frame_ref.x29, frame_ref.x30);
            crate::safe_print!(128, "[Fault]  SP_EL0={:#x} ELR={:#x} SPSR={:#x}\n",
                frame_ref.sp_el0, frame_ref.elr_el1, frame_ref.spsr_el1);
            if let Some(proc) = akuma_exec::process::current_process_shared() {
                let elapsed_us = (akuma_exec::runtime::runtime().uptime_us)()
                    .saturating_sub(proc.start_time_us);
                let secs = elapsed_us / 1_000_000;
                let frac = (elapsed_us % 1_000_000) / 10_000;
                crate::safe_print!(128, "[Fault] Process {} ({}) SIGSEGV after {}.{:02}s\n",
                    proc.pid, proc.name, secs, frac);
            }
            fatal_signal_group_exit(-11) // never returns
        }
        esr::EC_MSR_MRS_TRAP => {
            // Trapped MSR/MRS/System instruction from EL0.
            let direction = iss & 1; // 1 = MRS (read), 0 = MSR (write)
            let rt = ((iss >> 5) & 0x1F) as usize;
            let op0 = (iss >> 20) & 0x3;
            let op1 = (iss >> 14) & 0x7;
            let crn = (iss >> 10) & 0xF;
            let crm = (iss >> 1) & 0xF;
            let op2 = (iss >> 17) & 0x7;

            // Storm attribution (CROSS_CORE_THREAD_COLLAPSE.md §3): count the
            // first few DISTINCT trapped encodings so the >1M/s EC=0x18 storm
            // names its register. Printed on the [EXCC] heartbeat line.
            {
                let key = (op0 << 12) | (op1 << 9) | (crn << 5) | (crm << 1) | (op2 << 16) | direction;
                for slot in &MRS_TRAP_ENCODINGS {
                    let cur = slot.0.load(core::sync::atomic::Ordering::Relaxed);
                    if cur == key + 1 {
                        slot.1.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                    if cur == 0
                        && slot
                            .0
                            .compare_exchange(
                                0,
                                key + 1,
                                core::sync::atomic::Ordering::Relaxed,
                                core::sync::atomic::Ordering::Relaxed,
                            )
                            .is_ok()
                    {
                        slot.1.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                }
            }

            if direction == 1 && rt < 31 {
                // MRS (read) — emulate system register reads
                let value = if op0 == 3 && op1 == 3 && crn == 0 && crm == 0 && op2 == 1 {
                    // CTR_EL0
                    let ctr: u64;
                    unsafe { core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr); }
                    ctr
                } else if op0 == 3 && op1 == 3 && crn == 14 && crm == 0 && op2 == 2 {
                    // CNTVCT_EL0 — normally never trapped (CNTKCTL_EL1.EL0VCTEN
                    // is set at bringup); real-value fallback in case a build
                    // path misses the bit. Returning 0 here froze userspace's
                    // hardware clock (CROSS_CORE_THREAD_COLLAPSE.md §3).
                    let v: u64;
                    unsafe { core::arch::asm!("mrs {}, cntvct_el0", out(reg) v); }
                    v
                } else if op0 == 3 && op1 == 3 && crn == 14 && crm == 0 && op2 == 0 {
                    // CNTFRQ_EL0 — same fallback as CNTVCT above.
                    let v: u64;
                    unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) v); }
                    v
                } else {
                    0
                };
                unsafe {
                    let regs = frame.cast::<u64>();
                    core::ptr::write_volatile(regs.add(rt), value);
                }
            } else if direction == 0 {
                // MSR/DC/IC (write) — perform cache maintenance on behalf of user
                let addr = if rt < 31 {
                    unsafe { core::ptr::read_volatile((frame as *const u64).add(rt)) }
                } else {
                    0
                };
                if op0 == 1 && crn == 7 {
                    // Cache maintenance instruction (DC or IC).
                    // DC CVAU: op1=3, crm=11, op2=1
                    // IC IVAU: op1=3, crm=5, op2=1
                    // DC ZVA:  op1=3, crm=4,  op2=1
                    if op1 == 3 && crm == 11 && op2 == 1 {
                        unsafe { core::arch::asm!("dc cvau, {}", in(reg) addr); }
                    } else if op1 == 3 && crm == 5 && op2 == 1 {
                        unsafe { core::arch::asm!("ic ivau, {}", in(reg) addr); }
                    } else if op1 == 3 && crm == 4 && op2 == 1 {
                        emulate_dc_zva(addr);
                    }
                }
            }
            // Advance past the trapped instruction (always 4 bytes on AArch64)
            unsafe {
                let elr_ptr = &raw mut (*frame).elr_el1;
                let elr = core::ptr::read_volatile(elr_ptr);
                core::ptr::write_volatile(elr_ptr, elr + 4);
            }
            unsafe { (*frame).x0 }
        }
        esr::EC_BRK_AARCH64 => {
            // BRK instruction — intentional trap/abort from user code
            fatal_signal_group_exit(-5) // SIGTRAP
        }
        _ => {
            // EC=0x0 from EL0 is an undefined instruction. On Apple Silicon
            // under HVF/-cpu host, several optional AArch64 features that TCG
            // `-cpu max` implements are absent (FEAT_SM3/SM4/SVE/SVE2/…), so a
            // binary can reach one at runtime. The common case is a CPU-feature
            // *probe*: code deliberately executes the feature instruction inside
            // a SIGILL handler to detect support (OpenSSL's OPENSSL_cpuid_setup
            // armcaps, statically linked into nightly cargo via its git/curl
            // stack; libgcc's __init_cpu_features; etc.). Those rely on the
            // kernel delivering SIGILL to a registered userspace handler, the
            // way Linux does. Try that first; only fall through to a fatal
            // SIGILL when no handler is registered.
            let (elr, spsr) = unsafe { ((*frame).elr_el1, (*frame).spsr_el1) };
            crate::safe_print!(96, "[Exception] Unknown from EL0: EC={:#x}, ISS={:#x} ELR={:#x} — delivering SIGILL\n", ec, iss, elr);
            if try_deliver_signal(frame, 4, elr, true, esr) {
                return 4; // SIGILL delivered to a userspace handler
            }

            // No handler registered — fatal SIGILL. Capture additional state for
            // debugging. ELR/SPSR from the trap frame and FAR from the entry
            // snapshot — the live registers may belong to a later trap (see
            // rust_sync_el0_handler).
            let ttbr0: u64;
            let sp: u64;
            unsafe {
                core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr0);
                core::arch::asm!("mov {}, sp", out(reg) sp);
            }
            let tid = akuma_exec::threading::current_thread_id();

            crate::safe_print!(128, "  Thread={}, ELR={:#x}, FAR={:#x}, SPSR={:#x}\n", tid, elr, far, spsr);
            crate::safe_print!(64, "  TTBR0={:#x}, SP={:#x}\n", ttbr0, sp);

            // Check if this looks like a kernel TTBR0 (boot page tables)
            // Boot TTBR0 is typically around 0x43xxxxxx
            if ttbr0 & 0xFFFF_0000_0000_0000 == 0 && ttbr0 < 0x4400_0000 && ttbr0 > 0x4300_0000 {
                safe_print!(128, "  WARNING: TTBR0 looks like boot page tables, not user process!\n");
            }

            fatal_signal_group_exit(-4) // never returns; SIGILL
        }
    }
}
