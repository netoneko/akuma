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
    
    // Call Rust handler
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
// EL0 IRQ frame layout (832 bytes total):
//   [sp+0]:   x30 + padding           (GPR block: 304 bytes)
//   ...
//   [sp+288]: x10, x11
//   [sp+304]: Q0-Q31, FPCR, FPSR      (NEON block: 528 bytes)
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
//   [sp+0..303]:   GPR block (x30, x28-x29, ..., x0-x1, ELR, SPSR, SP_EL0, TPIDR)
//   [sp+304..831]: NEON block (Q0-Q31, FPCR, FPSR)
//   [sp+832..847]: x10, x11 (scratch, outermost)
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
    // Boot thread (thread 0) uses the boot stack at 0x40800000
    // Its exception stack is at the very top
    let boot_stack_top = 0x40800000u64;
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
// siginfo_t: 128 bytes
// ucontext_t header (uc_flags..uc_sigmask + __unused): 168 bytes
// sigcontext (fault_address + regs[31] + sp + pc + pstate): 280 bytes
// FPSIMD extension record: _aarch64_ctx(8) + fpsr(4) + fpcr(4) + vregs[32](512) = 528 bytes
// Null terminator _aarch64_ctx{0,0}: 8 bytes
const SIGFRAME_SIZE: usize = 128 + 168 + 280 + 528 + 8; // 1112 bytes
const SIGFRAME_SIGINFO: usize = 0;
const SIGFRAME_UCONTEXT: usize = 128;
const SIGFRAME_MCONTEXT: usize = SIGFRAME_UCONTEXT + 168; // 296
const SIGFRAME_FPSIMD: usize = SIGFRAME_MCONTEXT + 280;   // 576
const FPSIMD_MAGIC: u32 = 0x46508001;

// Exposed for kernel layout tests.
pub(crate) const TEST_SIGFRAME_SIZE: usize = SIGFRAME_SIZE;
pub(crate) const TEST_SIGFRAME_UCONTEXT: usize = SIGFRAME_UCONTEXT;
pub(crate) const TEST_SIGFRAME_MCONTEXT: usize = SIGFRAME_MCONTEXT;
pub(crate) const TEST_SIGFRAME_FPSIMD: usize = SIGFRAME_FPSIMD;
/// Byte offset of uc_sigmask within the signal frame (ucontext_t + 40).
pub(crate) const TEST_SIGFRAME_UC_SIGMASK: usize = SIGFRAME_UCONTEXT + 40;

/// True if `far` is in the kernel identity-RAM VA window (normally UXN for EL0 execute).
/// Used when deciding whether an EL0 instruction abort might be “stale translation” vs
/// a deliberate fault from jumping into kernel RAM.
#[inline]
pub(crate) fn far_in_kernel_identity_user_range(far: u64) -> bool {
    let a = far as usize;
    a >= akuma_exec::process::types::ProcessMemory::KERNEL_VA_START
        && a < akuma_exec::process::types::ProcessMemory::KERNEL_VA_END
}

/// Ensure a userspace page is mapped. If it's in a lazy anonymous region and
/// not yet mapped, allocates and maps a zeroed page. Returns true if the page
/// is mapped after this call (either was already mapped, or was just demand-paged).
fn ensure_user_page_mapped(pid: u32, page_va: usize) -> bool {
    if akuma_exec::mmu::is_current_user_page_mapped(page_va) {
        return true;
    }
    // Check if the page is in a lazy anonymous region
    if let Some((flags, source, _region_start, _region_size)) =
        akuma_exec::process::lazy_region_lookup_for_pid(pid, page_va)
    {
        // Only demand-page anonymous regions here; file-backed pages handled by the fault path
        // PROT_NONE regions must NOT be demand-paged — access should SIGSEGV.
        if akuma_exec::mmu::user_flags::is_none(flags) {
            return false;
        }
        if matches!(source, akuma_exec::process::LazySource::Zero) {
            let map_flags = if flags != 0 { flags } else { akuma_exec::mmu::user_flags::RW };
            if let Some(page_frame) = crate::pmm::alloc_page_zeroed() {
                let (table_frames, installed) = unsafe {
                    akuma_exec::mmu::map_user_page(page_va, page_frame.addr, map_flags)
                };
                if installed {
                    if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                        owner.address_space.track_user_frame(page_frame);
                        for tf in table_frames {
                            owner.address_space.track_page_table_frame(tf);
                        }
                    } else {
                        crate::pmm::free_page(page_frame);
                        for tf in table_frames { crate::pmm::free_page(tf); }
                    }
                } else {
                    crate::pmm::free_page(page_frame);
                    if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                        for tf in table_frames {
                            owner.address_space.track_page_table_frame(tf);
                        }
                    } else {
                        for tf in table_frames { crate::pmm::free_page(tf); }
                    }
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
    // movz x8, #139 = 0xD2801168 (LE: 68 11 80 D2)
    // svc  #0       = 0xD4000001 (LE: 01 00 00 D4)
    const TRAMPOLINE: [u8; 8] = [0x68, 0x11, 0x80, 0xD2, 0x01, 0x00, 0x00, 0xD4];

    if akuma_exec::mmu::is_current_user_page_mapped(SIGRETURN_TRAMPOLINE_ADDR) {
        return Some(SIGRETURN_TRAMPOLINE_ADDR);
    }

    let frame = crate::pmm::alloc_page_zeroed()?;
    unsafe {
        let ptr = akuma_exec::mmu::phys_to_virt(frame.addr) as *mut u8;
        core::ptr::copy_nonoverlapping(TRAMPOLINE.as_ptr(), ptr, TRAMPOLINE.len());
    }

    let (table_frames, installed) = unsafe {
        akuma_exec::mmu::map_user_page(SIGRETURN_TRAMPOLINE_ADDR, frame.addr, akuma_exec::mmu::user_flags::RX)
    };

    if let Some(owner) = akuma_exec::process::lookup_process(pid) {
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

/// Try to deliver a signal to a userspace handler by setting up an
/// rt_sigframe on the user stack and redirecting ELR to the handler.
/// Returns true if delivery succeeded (caller should return signal number as x0).
fn try_deliver_signal(frame: *mut UserTrapFrame, signal: u32, fault_addr: u64, is_fault: bool) -> bool {
    let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let proc = match akuma_exec::process::lookup_process(pid) {
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
    const SA_RESTART: u64 = 0x10000000;
    if action.flags & SA_RESTART != 0 {
        // Only if we were in a syscall (EC_SVC_LOWER)
        let esr: u64;
        unsafe { core::arch::asm!("mrs {}, esr_el1", out(reg) esr); }
        if (esr >> 26) == 0x15 { // EC_SVC_LOWER
            // Only restart the syscall if it was actually interrupted.
            // SA_RESTART must NOT apply to successful syscalls — backing up ELR
            // for a completed FUTEX_WAKE (ret=1) causes it to re-execute with
            // x0=1 (the return value), producing EINVAL (uaddr=1 is unaligned).
            let ret_val = unsafe { (*frame).x0 as i64 };
            if ret_val == -4 /* EINTR */ || ret_val == -512 /* ERESTARTSYS */ {
                unsafe { (*frame).elr_el1 -= 4; }
            }
        }
    }

    // When the process didn't register a restorer (Go on arm64 relies on the
    // vDSO instead of SA_RESTORER), lazily map our kernel-provided trampoline.
    let restorer = if action.restorer != 0 {
        action.restorer
    } else {
        match ensure_sigreturn_trampoline(pid) {
            Some(addr) => addr,
            None => {
                crate::tprint!(64, "[signal] failed to map sigreturn trampoline for pid={}\n", pid);
                return false;
            }
        }
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
    if last_page != first_page && !ensure_user_page_mapped(pid, last_page) {
        crate::tprint!(128, "[signal] sig {} frame page {:#x} not mappable\n", signal, last_page);
        return false;
    }

    unsafe {
        let base = new_sp as *mut u8;
        core::ptr::write_bytes(base, 0, SIGFRAME_SIZE);

        // siginfo_t
        let si = base.add(SIGFRAME_SIGINFO);
        core::ptr::write(si.add(0) as *mut i32, signal as i32);   // si_signo
        core::ptr::write(si.add(4) as *mut i32, 0i32);            // si_errno = 0
        core::ptr::write(si.add(8) as *mut i32,                   // si_code
            if is_fault { 1i32 } else { 0i32 });                  // SEGV_MAPERR=1, SI_USER=0
        core::ptr::write(si.add(16) as *mut u64, fault_addr);     // si_addr

        // ucontext.uc_stack (stack_t) — Go runtime reads this to determine
        // whether the signal arrived on the sigaltstack.  All-zero confuses
        // Go's panic recovery and can produce corrupted SP/PSTATE on sigreturn.
        let uc = base.add(SIGFRAME_UCONTEXT);
        let on_altstack = stack_top != user_sp;
        core::ptr::write(uc.add(16) as *mut u64, alt_sp);                   // ss_sp
        core::ptr::write(uc.add(24) as *mut i32,
            if on_altstack { 1i32 } else { 0i32 });                          // ss_flags (SS_ONSTACK=1)
        core::ptr::write(uc.add(32) as *mut u64, alt_size);                  // ss_size
        // uc_sigmask — save the signal mask *before* we block the delivered signal
        core::ptr::write(uc.add(40) as *mut u64, proc.signal_mask);          // uc_sigmask

        // mcontext_t (sigcontext) - Zeroed by write_bytes(base, 0, ...)
        let mc = base.add(SIGFRAME_MCONTEXT);
        core::ptr::write(mc as *mut u64, fault_addr);
        let regs_base = mc.add(8) as *mut u64;
        core::ptr::write(regs_base.add(0), frame_ref.x0);
        core::ptr::write(regs_base.add(1), frame_ref.x1);
        core::ptr::write(regs_base.add(2), frame_ref.x2);
        core::ptr::write(regs_base.add(3), frame_ref.x3);
        core::ptr::write(regs_base.add(4), frame_ref.x4);
        core::ptr::write(regs_base.add(5), frame_ref.x5);
        core::ptr::write(regs_base.add(6), frame_ref.x6);
        core::ptr::write(regs_base.add(7), frame_ref.x7);
        core::ptr::write(regs_base.add(8), frame_ref.x8);
        core::ptr::write(regs_base.add(9), frame_ref.x9);
        core::ptr::write(regs_base.add(10), frame_ref.x10);
        core::ptr::write(regs_base.add(11), frame_ref.x11);
        core::ptr::write(regs_base.add(12), frame_ref.x12);
        core::ptr::write(regs_base.add(13), frame_ref.x13);
        core::ptr::write(regs_base.add(14), frame_ref.x14);
        core::ptr::write(regs_base.add(15), frame_ref.x15);
        core::ptr::write(regs_base.add(16), frame_ref.x16);
        core::ptr::write(regs_base.add(17), frame_ref.x17);
        core::ptr::write(regs_base.add(18), frame_ref.x18);
        core::ptr::write(regs_base.add(19), frame_ref.x19);
        core::ptr::write(regs_base.add(20), frame_ref.x20);
        core::ptr::write(regs_base.add(21), frame_ref.x21);
        core::ptr::write(regs_base.add(22), frame_ref.x22);
        core::ptr::write(regs_base.add(23), frame_ref.x23);
        core::ptr::write(regs_base.add(24), frame_ref.x24);
        core::ptr::write(regs_base.add(25), frame_ref.x25);
        core::ptr::write(regs_base.add(26), frame_ref.x26);
        core::ptr::write(regs_base.add(27), frame_ref.x27);
        core::ptr::write(regs_base.add(28), frame_ref.x28);
        core::ptr::write(regs_base.add(29), frame_ref.x29);
        core::ptr::write(regs_base.add(30), frame_ref.x30);
        core::ptr::write(mc.add(256) as *mut u64, frame_ref.sp_el0);   // sp
        core::ptr::write(mc.add(264) as *mut u64, frame_ref.elr_el1);  // pc
        core::ptr::write(mc.add(272) as *mut u64, frame_ref.spsr_el1); // pstate

        // FPSIMD extension record at SIGFRAME_FPSIMD.
        // sync_el0_handler saves Q0-Q31 at frame+304 (16 bytes each), fpcr at frame+816,
        // fpsr at frame+824.  The kernel never uses FP so those values are the user's.
        let kernel_neon = (frame as *const u8).add(304);
        let fp = base.add(SIGFRAME_FPSIMD);
        core::ptr::write(fp as *mut u32, FPSIMD_MAGIC);       // _aarch64_ctx.magic
        core::ptr::write(fp.add(4) as *mut u32, 528u32);      // _aarch64_ctx.size
        // fpsr at +8, fpcr at +12 (stored as 64-bit on kernel stack, lower 32 bits used)
        let fpsr_val = core::ptr::read((frame as *const u8).add(824) as *const u32);
        let fpcr_val = core::ptr::read((frame as *const u8).add(816) as *const u32);
        core::ptr::write(fp.add(8) as *mut u32, fpsr_val);
        core::ptr::write(fp.add(12) as *mut u32, fpcr_val);
        // vregs[0..31]: 16 bytes each
        let vregs_dst = fp.add(16);
        for i in 0..32usize {
            let src = kernel_neon.add(i * 16) as *const u128;
            let dst = vregs_dst.add(i * 16) as *mut u128;
            core::ptr::write(dst, core::ptr::read(src));
        }
        // Null terminator _aarch64_ctx{0,0}
        let null_term = fp.add(528);
        core::ptr::write(null_term as *mut u64, 0u64);

        // Redirect execution to the signal handler
        (*frame).elr_el1 = handler_addr as u64;
        (*frame).sp_el0 = new_sp as u64;
        (*frame).x30 = restorer as u64;

        // Demand-paged or RW-only mappings can leave the handler/restorer pages
        // non-executable; without RX, ERET to the handler faults immediately.
        // (If fault_pc is in the 0x6000_0000 kernel-RAM VA range, that is a
        // separate bug: user tried to *execute* identity-mapped RAM — usually UXN.)
        let handler_va = handler_addr & !0xFFF;
        let _ = proc.address_space.update_page_flags(handler_va, akuma_exec::mmu::user_flags::RX);
        let restorer_va = restorer & !0xFFF;
        let _ = proc.address_space.update_page_flags(restorer_va, akuma_exec::mmu::user_flags::RX);
        proc.address_space.invalidate_icache_for_page_va(handler_va);
        proc.address_space.invalidate_icache_for_page_va(restorer_va);

        if action.flags & SA_SIGINFO != 0 {
            (*frame).x1 = (new_sp + SIGFRAME_SIGINFO) as u64;
            (*frame).x2 = (new_sp + SIGFRAME_UCONTEXT) as u64;
        }
    }

    // Block the delivered signal during handler execution unless SA_NODEFER is set.
    // This prevents recursive delivery of the same signal.
    if action.flags & SA_NODEFER == 0 && signal >= 1 && signal <= 64 {
        if signal != 9 && signal != 19 { // SIGKILL/SIGSTOP cannot be masked
            proc.signal_mask |= 1u64 << (signal - 1);
        }
    }

    crate::tprint!(128, "[signal] Delivering sig {} to handler {:#x} (restorer={:#x})\n",
        signal, handler_addr, restorer);
    true
}
/// Restore saved context from a signal frame on the user stack (rt_sigreturn).
/// Returns the saved x0 value, or None if the frame is invalid.
fn do_rt_sigreturn(frame: *mut UserTrapFrame) -> Option<u64> {
    let frame_ref = unsafe { &*frame };
    let sigframe_sp = frame_ref.sp_el0 as usize;

    let first_page = sigframe_sp & !0xFFF;
    let last_page = (sigframe_sp + SIGFRAME_SIZE - 1) & !0xFFF;
    if !akuma_exec::mmu::is_current_user_page_mapped(first_page) {
        return None;
    }
    if last_page != first_page && !akuma_exec::mmu::is_current_user_page_mapped(last_page) {
        return None;
    }

    unsafe {
        let mc = (sigframe_sp + SIGFRAME_MCONTEXT) as *const u8;
        let regs_base = mc.add(8) as *const u64;

        (*frame).x0 = core::ptr::read(regs_base.add(0));
        (*frame).x1 = core::ptr::read(regs_base.add(1));
        (*frame).x2 = core::ptr::read(regs_base.add(2));
        (*frame).x3 = core::ptr::read(regs_base.add(3));
        (*frame).x4 = core::ptr::read(regs_base.add(4));
        (*frame).x5 = core::ptr::read(regs_base.add(5));
        (*frame).x6 = core::ptr::read(regs_base.add(6));
        (*frame).x7 = core::ptr::read(regs_base.add(7));
        (*frame).x8 = core::ptr::read(regs_base.add(8));
        (*frame).x9 = core::ptr::read(regs_base.add(9));
        (*frame).x10 = core::ptr::read(regs_base.add(10));
        (*frame).x11 = core::ptr::read(regs_base.add(11));
        (*frame).x12 = core::ptr::read(regs_base.add(12));
        (*frame).x13 = core::ptr::read(regs_base.add(13));
        (*frame).x14 = core::ptr::read(regs_base.add(14));
        (*frame).x15 = core::ptr::read(regs_base.add(15));
        (*frame).x16 = core::ptr::read(regs_base.add(16));
        (*frame).x17 = core::ptr::read(regs_base.add(17));
        (*frame).x18 = core::ptr::read(regs_base.add(18));
        (*frame).x19 = core::ptr::read(regs_base.add(19));
        (*frame).x20 = core::ptr::read(regs_base.add(20));
        (*frame).x21 = core::ptr::read(regs_base.add(21));
        (*frame).x22 = core::ptr::read(regs_base.add(22));
        (*frame).x23 = core::ptr::read(regs_base.add(23));
        (*frame).x24 = core::ptr::read(regs_base.add(24));
        (*frame).x25 = core::ptr::read(regs_base.add(25));
        (*frame).x26 = core::ptr::read(regs_base.add(26));
        (*frame).x27 = core::ptr::read(regs_base.add(27));
        (*frame).x28 = core::ptr::read(regs_base.add(28));
        (*frame).x29 = core::ptr::read(regs_base.add(29));
        (*frame).x30 = core::ptr::read(regs_base.add(30));

        (*frame).sp_el0 = core::ptr::read(mc.add(256) as *const u64);
        (*frame).elr_el1 = core::ptr::read(mc.add(264) as *const u64);
        (*frame).spsr_el1 = core::ptr::read(mc.add(272) as *const u64);

        crate::tprint!(256,
            "[sigreturn] restoring: sp={:#x} pc={:#x} pstate={:#x} sigframe_sp={:#x}\n",
            (*frame).sp_el0, (*frame).elr_el1, (*frame).spsr_el1, sigframe_sp);

        // Restore signal mask from uc_sigmask (ucontext+40)
        let uc_sigmask_ptr = (sigframe_sp + SIGFRAME_UCONTEXT + 40) as *const u64;
        let saved_mask = core::ptr::read(uc_sigmask_ptr);
        if let Some(proc) = akuma_exec::process::current_process() {
            // Never block SIGKILL (bit 8) or SIGSTOP (bit 18)
            proc.signal_mask = saved_mask & !((1u64 << 8) | (1u64 << 18));
        }

        // Restore FPSIMD state from signal frame into kernel stack NEON save area.
        // sync_el0_handler will restore NEON from frame+304 after rust_sync_el0_handler returns.
        let fp = (sigframe_sp + SIGFRAME_FPSIMD) as *const u8;
        let magic = core::ptr::read(fp as *const u32);
        if magic == FPSIMD_MAGIC {
            let fpsr_val = core::ptr::read(fp.add(8) as *const u32) as u64;
            let fpcr_val = core::ptr::read(fp.add(12) as *const u32) as u64;
            core::ptr::write((frame as *mut u8).add(824) as *mut u64, fpsr_val);
            core::ptr::write((frame as *mut u8).add(816) as *mut u64, fpcr_val);
            let vregs_src = fp.add(16);
            let kernel_neon = (frame as *mut u8).add(304);
            for i in 0..32usize {
                let src = vregs_src.add(i * 16) as *const u128;
                let dst = kernel_neon.add(i * 16) as *mut u128;
                core::ptr::write(dst, core::ptr::read(src));
            }
        }

        let saved_x0 = (*frame).x0;
        Some(saved_x0)
    }
}

/// Install exception vector table
pub fn init() {
    // Initialize exception stack before enabling exceptions
    init_exception_stack();

    unsafe {
        let vbar = &exception_vector_table as *const _ as u64;

        // Set VBAR_EL1 (Vector Base Address Register)
        core::arch::asm!(
            "msr vbar_el1, {vbar}",
            "isb",
            vbar = in(reg) vbar
        );

        // Enable IRQs by clearing the I bit in DAIF
        core::arch::asm!(
            "msr daifclr, #2" // Clear IRQ mask (bit 1)
        );
    }
}

/// Default exception handler - logs unexpected exceptions
/// CRITICAL: Must NOT return if ELR/SPSR are invalid, or ERET will crash!
#[unsafe(no_mangle)]
extern "C" fn rust_default_exception_handler() {
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
#[unsafe(no_mangle)]
extern "C" fn rust_irq_handler_with_sp(current_sp: u64) -> u64 {
    // Acknowledge the interrupt and get IRQ number
    let irq_opt = crate::gic::acknowledge_irq();
    
    if let Some(irq) = irq_opt {
        // Special handling for scheduler SGI
        if irq == crate::gic::SGI_SCHEDULER {
            // Returns new SP if switch needed, or 0 if not
            return akuma_exec::threading::sgi_scheduler_handler_with_sp(irq, current_sp);
        } else {
            // Normal IRQs: call handler then EOI
            crate::irq::dispatch_irq(irq);
            crate::gic::end_of_interrupt(irq);
        }
    }
    0  // No context switch
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

/// Synchronous exception handler from EL1 (kernel mode)
/// Uses static buffers to avoid heap allocation during crash
#[unsafe(no_mangle)]
extern "C" fn rust_sync_el1_handler() {
    // #region agent log
    crate::console::print("[FORK-DBG] EL1 SYNC EXCEPTION!\n");
    // #endregion
    use core::fmt::Write;
    
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

    // Use static buffer for formatting (no heap allocation)
    let mut w = StaticWriter::new();
    
    let _ = write!(w, "[Exception] Sync from EL1: EC={:#x}, ISS={:#x}\n", ec, iss);
    w.flush();
    let _ = write!(w, "  ELR={:#x}, FAR={:#x}, SPSR={:#x}\n", elr, far, spsr);
    w.flush();
    let _ = write!(w, "  Thread={}, TTBR0={:#x}, TTBR1={:#x}\n", tid, ttbr0, ttbr1);
    w.flush();
    let _ = write!(w, "  SP={:#x}, SP_EL0={:#x}\n", sp, sp_el0);
    w.flush();
    
    // Try to read the faulting instruction (if ELR is in kernel range)
    if elr >= 0x4000_0000 && elr < 0x8000_0000 {
        let instr = unsafe { *(elr as *const u32) };
        let _ = write!(w, "  Instruction at ELR: {:#010x}\n", instr);
        w.flush();
        
        // Decode ARM64 load/store instruction to find base register
        // LDR/STR format: opc[31:30] | 111 | V[26] | 00 | opc2[23:22] | imm9 | op[11:10] | Rn[9:5] | Rt[4:0]
        // Or: opc[31:30] | 111 | V[26] | 01 | opc2[23:22] | imm12[21:10] | Rn[9:5] | Rt[4:0]
        let rn = ((instr >> 5) & 0x1F) as usize;
        let rt = (instr & 0x1F) as usize;
        let _ = write!(w, "  Likely: Rn(base)=x{}, Rt(dest)=x{}\n", rn, rt);
        w.flush();
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
        // Kernel is loaded at RAM_BASE+2MB (0x4020_0000) and extends to ~0x6000_0000.
        let in_kernel_code = elr >= 0x4020_0000 && elr < 0x6000_0000;
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

            if far >= 0x4000_0000 && far < 0x8000_0000 {
                let _ = write!(w, "  HINT: FAR={:#x} is in kernel identity-mapped RAM range.\n", far);
                w.flush();
                let _ = write!(w, "  Likely cause: phys_to_virt() write to a physical page whose VA\n");
                w.flush();
                let _ = write!(w, "  is not mapped in the current user page tables (TTBR0).\n");
                w.flush();
            }
            let _ = write!(w, "  EC=0x25 in kernel code — killing current process (EFAULT)\n");
            w.flush();
            if let Some(proc) = akuma_exec::process::current_process() {
                let _ = write!(w, "  Killing PID {} ({})\n", proc.pid, proc.name);
                w.flush();
                let l0_phys = proc.address_space.l0_phys();
                let pid = proc.pid;
                proc.exited = true;
                proc.exit_code = -14; // EFAULT
                proc.state = akuma_exec::process::ProcessState::Zombie(-14);
                akuma_exec::process::kill_thread_group(pid, l0_phys);
                // Wake any CLONE_VFORK parent waiting for this child to exec/exit.
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
        let _ = write!(w, "    Expected boot_ttbr0: {:#x}\n", boot_ttbr0);
        w.flush();
        let _ = write!(w, "    Current TTBR0:       {:#x}\n", ttbr0);
        w.flush();
        
        if ttbr0 != boot_ttbr0 {
            safe_print!(64, "    WARNING: TTBR0 mismatch!\n");
        }
        
        // Read L0[0] entry to check if it points to valid L1
        let l0_base = ttbr0 & !0xFFF; // Mask off ASID etc
        let l0_entry = unsafe { *(l0_base as *const u64) };
        let _ = write!(w, "    L0[0] entry: {:#018x}\n", l0_entry);
        w.flush();
        
        // Check if L0[0] looks valid (should be table descriptor)
        let is_valid = (l0_entry & 0x1) == 1;
        let is_table = (l0_entry & 0x2) == 2;
        let l1_addr = l0_entry & 0x0000_FFFF_FFFF_F000;
        let _ = write!(w, "    L0[0]: valid={}, table={}, L1_addr={:#x}\n", 
            is_valid, is_table, l1_addr);
        w.flush();
        
        // Expected L1 address should be boot_ttbr0 + 8192 (2 pages)
        let expected_l1 = boot_ttbr0 + 8192;
        let _ = write!(w, "    Expected L1 addr: {:#x}\n", expected_l1);
        w.flush();
        
        if l1_addr != expected_l1 {
            safe_print!(128, "    WARNING: L1 address mismatch - page table corrupted!\n");
        }
        
        // Now read L1[0] to check the device memory block entry
        if is_valid && is_table && l1_addr >= 0x4000_0000 && l1_addr < 0x8000_0000 {
            let l1_entry = unsafe { *(l1_addr as *const u64) };
            let _ = write!(w, "    L1[0] entry: {:#018x}\n", l1_entry);
            w.flush();
            
            // L1[0] should be a 1GB block descriptor for device memory
            // Valid block: bits[1:0] = 01, bits[47:30] = physical address
            let is_l1_valid = (l1_entry & 0x1) == 1;
            let is_block = (l1_entry & 0x2) == 0; // Block, not table
            let block_addr = l1_entry & 0x0000_FFFF_C000_0000;
            let _ = write!(w, "    L1[0]: valid={}, block={}, phys_addr={:#x}\n", 
                is_l1_valid, is_block, block_addr);
            w.flush();
            
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

// ============================================================================
// Static buffer formatting for crash handlers (no heap allocation)
// ============================================================================

/// Static buffer writer for crash-safe formatting
/// Uses a fixed-size buffer on the stack to avoid heap allocations
struct StaticWriter {
    buf: [u8; 256],
    pos: usize,
}

impl StaticWriter {
    fn new() -> Self {
        Self {
            buf: [0u8; 256],
            pos: 0,
        }
    }
    
    fn as_str(&self) -> &str {
        // Safety: we only write valid UTF-8 via core::fmt::Write
        unsafe { core::str::from_utf8_unchecked(&self.buf[..self.pos]) }
    }
    
    fn clear(&mut self) {
        self.pos = 0;
    }
    
    /// Write and flush to console, then clear buffer
    fn flush(&mut self) {
        if self.pos > 0 {
            crate::console::print(self.as_str());
            self.clear();
        }
    }
}

impl core::fmt::Write for StaticWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let remaining = self.buf.len() - self.pos;
        let to_write = bytes.len().min(remaining);
        if to_write > 0 {
            self.buf[self.pos..self.pos + to_write].copy_from_slice(&bytes[..to_write]);
            self.pos += to_write;
        }
        Ok(()) // Always succeed, just truncate if full
    }
}

/// Log comprehensive memory stats when a crash occurs
/// Uses static buffer to avoid heap allocations during crash
fn log_memory_stats_on_crash(tid: usize, kernel_sp: u64, user_sp: u64) {
    use core::fmt::Write;
    let mut w = StaticWriter::new();
    
    safe_print!(64, "\n=== Memory Stats at Crash ===\n");
    
    // Kernel heap stats
    let heap_stats = crate::allocator::stats();
    let _ = write!(w, "  Heap: {}/{} bytes used ({} allocs, peak={})\n",
        heap_stats.allocated,
        heap_stats.heap_size,
        heap_stats.allocation_count,
        heap_stats.peak_allocated
    );
    w.flush();
    
    // PMM stats
    let pmm_free = crate::pmm::free_count();
    let pmm_total = crate::pmm::total_count();
    let _ = write!(w, "  PMM: {}/{} pages free ({} KB / {} KB)\n",
        pmm_free, pmm_total,
        pmm_free * 4, pmm_total * 4
    );
    w.flush();
    
    // Frame tracking stats if enabled
    if let Some(frame_stats) = crate::pmm::tracking_stats() {
        let _ = write!(w, "  Frames: kernel={}, user_pt={}, user_data={}, elf={}\n",
            frame_stats.kernel_count,
            frame_stats.user_page_table_count,
            frame_stats.user_data_count,
            frame_stats.elf_loader_count
        );
        w.flush();
    }
    
    // Thread stack info
    let (thread_count, running, terminated) = akuma_exec::threading::thread_stats();
    let _ = write!(w, "  Threads: {} total, {} running, {} terminated\n",
        thread_count, running, terminated
    );
    w.flush();
    
    // Current thread's kernel stack info
    if let Some(stack_info) = akuma_exec::threading::get_thread_stack_info(tid) {
        let kernel_stack_used = if kernel_sp >= stack_info.0 as u64 && kernel_sp <= stack_info.1 as u64 {
            stack_info.1 - kernel_sp as usize
        } else {
            0 // SP outside expected range
        };
        let _ = write!(w, "  Thread {} kernel stack: base={:#x}, top={:#x}\n",
            tid, stack_info.0, stack_info.1
        );
        w.flush();
        let _ = write!(w, "    SP={:#x}, used={} bytes\n", kernel_sp, kernel_stack_used);
        w.flush();
        if kernel_sp < stack_info.0 as u64 || kernel_sp > stack_info.1 as u64 {
            safe_print!(128, "  WARNING: Kernel SP outside thread's stack bounds!\n");
        }
    }
    
    // User process info (if any)
    if let Some(proc) = akuma_exec::process::current_process() {
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
        let _ = write!(w, "  Process PID={} '{}'\n", proc.pid, proc.name);
        w.flush();
        
        let _ = write!(w, "    Stack: {:#x}-{:#x} ({} KB)\n",
            mem.stack_bottom, mem.stack_top, stack_size / 1024
        );
        w.flush();
        
        // Calculate percentage without floating point (integer percentage)
        let stack_pct = if stack_size > 0 { (stack_used * 100) / stack_size } else { 0 };
        let _ = write!(w, "    SP_EL0={:#x}, used={} bytes ({}%)\n",
            user_sp, stack_used, stack_pct
        );
        w.flush();
        
        let _ = write!(w, "    Heap: brk={:#x} (initial={:#x}), grown={} bytes\n",
            proc.brk, proc.initial_brk, heap_used
        );
        w.flush();
        
        let _ = write!(w, "    Mmap: next={:#x}, limit={:#x}, used={} bytes\n",
            mem.next_mmap.load(core::sync::atomic::Ordering::Relaxed), mem.mmap_limit, mmap_used
        );
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

/// Synchronous exception handler from EL0 (user mode)
/// Returns the syscall return value, or doesn't return if process exits
#[unsafe(no_mangle)]
extern "C" fn rust_sync_el0_handler(frame: *mut UserTrapFrame) -> u64 {
    // Read ESR_EL1 to determine exception type
    let esr: u64;
    unsafe {
        core::arch::asm!("mrs {}, esr_el1", out(reg) esr);
    }

    let ec = (esr >> 26) & 0x3F; // Exception Class
    let iss = esr & 0x1FFFFFF; // Instruction Specific Syndrome

    match ec {
        esr::EC_SVC64 => {
            // System call - number in x8, args in x0-x5
            let frame_ref = unsafe { &*frame };
            let syscall_num = frame_ref.x8;

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
                        let prev_instr = elr.checked_sub(4).and_then(|prev_va| {
                            let mut buf = [0u8; 4];
                            unsafe {
                                akuma_exec::mmu::user_access::copy_from_user_safe(
                                    buf.as_mut_ptr(), prev_va as *const u8, 4,
                                ).ok()
                            }
                            .map(|_| u32::from_le_bytes(buf))
                        });
                        let prev_is_svc = prev_instr
                            .map(|instr| (instr & 0xFFE0001F) == 0xD4000001)
                            .unwrap_or(false);
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
                        let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
                        let sig_mask = if let Some(p) = akuma_exec::process::lookup_process(pid) {
                            p.signal_mask
                        } else {
                            0
                        };
                        // Block fault signals in this path by adding them to the effective mask.
                        let effective_mask = sig_mask | FAULT_SIGNALS;
                        if let Some(sig) = akuma_exec::threading::take_pending_signal(effective_mask) {
                            unsafe { (*frame).x0 = frame_ref.x0; }
                            if try_deliver_signal(frame, sig, 0, false) {
                                return sig as u64;
                            }
                        }
                        return frame_ref.x0;
                    }
                    crate::safe_print!(128, "[JIT] giving up after {} retries, nr={}\n",
                        count + 1, syscall_num);
                    JIT_RETRY_COUNT.store(0, Ordering::Relaxed);
                } else {
                    JIT_RETRY_COUNT.store(0, Ordering::Relaxed);
                }
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
                    let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
                    let sig_mask = if let Some(p) = akuma_exec::process::lookup_process(pid) {
                        p.signal_mask
                    } else {
                        0
                    };
                    if let Some(sig) = akuma_exec::threading::take_pending_signal(sig_mask) {
                        unsafe { (*frame).x0 = saved_x0; }
                        if try_deliver_signal(frame, sig, 0, false) {
                            return sig as u64;
                        }
                    }
                    return saved_x0;
                }
                if let Some(pid) = akuma_exec::process::read_current_pid() {
                    crate::syscall::proc::vfork_complete(pid);
                }
                akuma_exec::process::return_to_kernel(-11);
            }

            // Save trap frame pointer so fork/clone can read full register state
            akuma_exec::threading::set_current_trap_frame(frame as *const _);
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
            if let Some(proc) = akuma_exec::process::current_process() {
                if proc.exited {
                    let exit_code = proc.exit_code;
                    
                    // Validate exit code - detect corruption (pointer-like values)
                    let exit_code_u32 = exit_code as u32;
                    if exit_code_u32 >= 0x40000000 && exit_code_u32 < 0x50000000 {
                        safe_print!(128, "[exception] CORRUPT EXIT CODE DETECTED!\n");
                        crate::safe_print!(128, "  PID={}, exit_code={} (0x{:x}) looks like kernel address\n",
                            proc.pid, exit_code, exit_code_u32);
                        crate::safe_print!(96, "  proc ptr=0x{:x}, &exit_code=0x{:x}\n",
                            proc as *const _ as usize, 
                            &proc.exit_code as *const _ as usize);
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
            let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
            let sig_mask = if let Some(p) = akuma_exec::process::lookup_process(pid) {
                p.signal_mask
            } else {
                0
            };

            if let Some(sig) = akuma_exec::threading::take_pending_signal(sig_mask) {
                // Store the syscall return value in x0 of the trap frame so that
                // sigreturn restores it correctly (the signal handler's x0 = sig,
                // and after sigreturn the caller sees x0 = syscall result).
                unsafe { (*frame).x0 = ret; }
                if try_deliver_signal(frame, sig, 0, false) {
                    return sig as u64; // x0 = signal number for the handler
                }
                // Delivery failed (no handler / bad stack); just return normally.
            }

            ret
        }
        esr::EC_DATA_ABORT_LOWER => {
            let far: u64;
            let elr: u64;
            unsafe {
                core::arch::asm!("mrs {}, far_el1", out(reg) far);
                core::arch::asm!("mrs {}, elr_el1", out(reg) elr);
            }

            let pid = akuma_exec::process::read_current_pid().unwrap_or(0);

            // Translation/permission fault (ISS bits [5:2]) — try demand paging
            let fault_type = iss & 0x3C; // DFSC[5:2]
            let is_translation_fault = fault_type == 0x04 || fault_type == 0x08;
            let is_permission_fault = fault_type == 0x0C;
            let far_usize = far as usize;

            if is_permission_fault {
                if let Some((region_flags, _source, _region_start, _region_size)) = akuma_exec::process::lazy_region_lookup_for_pid(pid, far_usize) {
                    if !akuma_exec::mmu::user_flags::is_none(region_flags) {
                        let page_va = far_usize & !(0xFFF);
                        if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                            let _ = owner.address_space.update_page_flags(page_va, akuma_exec::mmu::user_flags::RW_NO_EXEC);
                            return unsafe { (*frame).x0 };
                        }
                    }
                }
            }

            if is_translation_fault {
                if let Some((flags, source, region_start, region_size)) = akuma_exec::process::lazy_region_lookup_for_pid(pid, far_usize) {
                    if akuma_exec::mmu::user_flags::is_none(flags) {
                        // PROT_NONE: don't demand-page, fall through to SIGSEGV
                    } else if far_in_kernel_identity_user_range(far) {
                        // Fault VA is in the kernel identity-map range — demand-paging here
                        // would corrupt kernel memory.  Fall through to SIGSEGV.
                        crate::tprint!(128, "[DA-DP] pid={} fault in kernel VA range {:#x} -> SIGSEGV\n",
                            pid, far_usize);
                    } else {
                    let page_va = far_usize & !(0xFFF);
                    let map_flags = match source {
                        akuma_exec::process::LazySource::File { .. } => {
                            if flags != 0 { flags } else { akuma_exec::mmu::user_flags::RW_NO_EXEC }
                        }
                        _ => akuma_exec::mmu::user_flags::RW_NO_EXEC,
                    };
                    let is_exec = (map_flags & akuma_exec::mmu::flags::UXN) == 0;

                    if let akuma_exec::process::LazySource::File { ref path, inode, file_offset, filesz, segment_va } = source {
                        const READAHEAD_PAGES: usize = 256;
                        let region_end = region_start + region_size;
                        let ra_end = core::cmp::min(page_va + READAHEAD_PAGES * 0x1000, region_end);

                        // Count how many pages actually need allocation (skip mapped)
                        let mut needed = 0usize;
                        {
                            let mut va = page_va;
                            while va < ra_end {
                                if !akuma_exec::mmu::is_current_user_page_mapped(va) {
                                    needed += 1;
                                }
                                va += 0x1000;
                            }
                        }

                        // Batch-allocate all needed frames in one lock acquisition
                        let frame_pool = if needed > 0 {
                            crate::pmm::alloc_pages_zeroed(needed).unwrap_or_else(|| {
                                // Fallback: allocate what we can one at a time
                                let mut v = alloc::vec::Vec::new();
                                for _ in 0..needed {
                                    match crate::pmm::alloc_page_zeroed() {
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

                        let mut any_mapped = false;
                        let mut pages_mapped = 0u64;
                        let mut cur_va = page_va;
                        while cur_va < ra_end {
                            if akuma_exec::mmu::is_current_user_page_mapped(cur_va) {
                                cur_va += 0x1000;
                                continue;
                            }
                            if pool_idx >= frame_pool.len() {
                                break;
                            }
                            let pf = frame_pool[pool_idx];
                            pool_idx += 1;

                            {
                                let pg_data_start = core::cmp::max(cur_va, segment_va);
                                let pg_data_end = core::cmp::min(cur_va + 0x1000, segment_va + filesz);
                                if pg_data_start < pg_data_end {
                                    let dst_off = pg_data_start - cur_va;
                                    let file_off = file_offset + (pg_data_start - segment_va);
                                    let len = pg_data_end - pg_data_start;
                                    let page_ptr = akuma_exec::mmu::phys_to_virt(pf.addr);
                                    let page_buf = unsafe {
                                        core::slice::from_raw_parts_mut((page_ptr as *mut u8).add(dst_off), len)
                                    };
                                    if inode != 0 {
                                        let _ = crate::vfs::read_at_by_inode(path, inode, file_off, page_buf);
                                    } else {
                                        let _ = crate::vfs::read_at(path, file_off, page_buf);
                                    }
                                }
                            }

                                if is_exec {
                                    let kva = akuma_exec::mmu::phys_to_virt(pf.addr) as usize;
                                    for off in (0..0x1000_usize).step_by(64) {
                                        unsafe { core::arch::asm!("dc cvau, {}", in(reg) kva + off); }
                                    }
                                    unsafe { core::arch::asm!("dsb ish"); }
                                    for off in (0..0x1000_usize).step_by(64) {
                                        unsafe { core::arch::asm!("ic ivau, {}", in(reg) cur_va + off); }
                                    }
                                }

                                // Use no_flush variant — we batch the TLB invalidation
                                // after the loop with a single flush_tlb_range call.
                                let (table_frames, installed) = unsafe {
                                    akuma_exec::mmu::map_user_page_no_flush(cur_va, pf.addr, map_flags)
                                };
                                if installed {
                                    if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                                        owner.address_space.track_user_frame(pf);
                                        for tf in table_frames {
                                            owner.address_space.track_page_table_frame(tf);
                                        }
                                    } else {
                                        crate::pmm::free_page(pf);
                                        for tf in table_frames { crate::pmm::free_page(tf); }
                                    }
                                    any_mapped = true;
                                    pages_mapped += 1;
                                } else {
                                    // Race: another CPU mapped this page between our check and
                                    // the atomic install. The page IS mapped now - don't SIGSEGV!
                                    crate::pmm::free_page(pf);
                                    if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                                        for tf in table_frames {
                                            owner.address_space.track_page_table_frame(tf);
                                        }
                                    } else {
                                        for tf in table_frames { crate::pmm::free_page(tf); }
                                    }
                                    // Critical: if this is the faulting page, it's now mapped!
                                    if cur_va == page_va {
                                        any_mapped = true;
                                    }
                                }
                            cur_va += 0x1000;
                        }

                        // Flush TLB for the entire readahead range in one shot.
                        // This replaces N individual (dsb+tlbi+dsb+isb) sequences with
                        // a single flush_tlb_range call — ~100x fewer barriers for 256 pages.
                        if any_mapped {
                            akuma_exec::mmu::flush_tlb_range(page_va, pages_mapped as usize);
                        }

                        // Return unused frames from the batch back to PMM
                        while pool_idx < frame_pool.len() {
                            crate::pmm::free_page(frame_pool[pool_idx]);
                            pool_idx += 1;
                        }

                        if is_exec {
                            unsafe {
                                core::arch::asm!("dsb ish");
                                core::arch::asm!("isb");
                            }
                        }

                        if any_mapped {
                            crate::syscall::syscall_counters::inc_pagefault(pages_mapped);
                            if crate::config::PROCESS_SYSCALL_STATS {
                                if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                                    owner.syscall_stats.inc_pagefault(pages_mapped);
                                }
                            }
                            return unsafe { (*frame).x0 };
                        } else if akuma_exec::mmu::is_current_user_page_mapped(page_va) {
                            // Race: another CPU mapped the faulting page while we were doing
                            // readahead. The page is now present — return success.
                            return unsafe { (*frame).x0 };
                        } else {
                            // Readahead pool was exhausted before reaching page_va.
                            // Fall back to a single-page allocation for just the faulting page.
                            let (_, _, free) = crate::pmm::stats();
                            crate::tprint!(128, "[DA-DP] pid={} va=0x{:x} readahead pool exhausted, {} free pages — retrying single page\n",
                                pid, far_usize, free);
                            if let Some(pf) = crate::pmm::alloc_page_zeroed() {
                                // Re-read file data for this single page
                                let pg_data_start = core::cmp::max(page_va, segment_va);
                                let pg_data_end = core::cmp::min(page_va + 0x1000, segment_va + filesz);
                                if pg_data_start < pg_data_end {
                                    let dst_off = pg_data_start - page_va;
                                    let file_off = file_offset + (pg_data_start - segment_va);
                                    let len = pg_data_end - pg_data_start;
                                    let page_ptr = akuma_exec::mmu::phys_to_virt(pf.addr);
                                    let page_buf = unsafe {
                                        core::slice::from_raw_parts_mut((page_ptr as *mut u8).add(dst_off), len)
                                    };
                                    if inode != 0 {
                                        let _ = crate::vfs::read_at_by_inode(path, inode, file_off, page_buf);
                                    } else {
                                        let _ = crate::vfs::read_at(path, file_off, page_buf);
                                    }
                                }
                                if is_exec {
                                    let kva = akuma_exec::mmu::phys_to_virt(pf.addr) as usize;
                                    for off in (0..0x1000_usize).step_by(64) {
                                        unsafe { core::arch::asm!("dc cvau, {}", in(reg) kva + off); }
                                    }
                                    unsafe { core::arch::asm!("dsb ish"); }
                                    for off in (0..0x1000_usize).step_by(64) {
                                        unsafe { core::arch::asm!("ic ivau, {}", in(reg) page_va + off); }
                                    }
                                    unsafe { core::arch::asm!("dsb ish"); core::arch::asm!("isb"); }
                                }
                                let (table_frames, installed) = unsafe {
                                    akuma_exec::mmu::map_user_page(page_va, pf.addr, map_flags)
                                };
                                if installed {
                                    if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                                        owner.address_space.track_user_frame(pf);
                                        for tf in table_frames { owner.address_space.track_page_table_frame(tf); }
                                    } else {
                                        crate::pmm::free_page(pf);
                                        for tf in table_frames { crate::pmm::free_page(tf); }
                                    }
                                } else {
                                    crate::pmm::free_page(pf);
                                    if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                                        for tf in table_frames { owner.address_space.track_page_table_frame(tf); }
                                    } else {
                                        for tf in table_frames { crate::pmm::free_page(tf); }
                                    }
                                }
                                crate::syscall::syscall_counters::inc_pagefault(1);
                                return unsafe { (*frame).x0 };
                            } else {
                                let (_, _, free2) = crate::pmm::stats();
                                crate::tprint!(128, "[DA-DP] pid={} va=0x{:x} single-page fallback OOM, {} free pages\n",
                                    pid, far_usize, free2);
                            }
                        }
                    } else {
                        if let Some(page_frame) = crate::pmm::alloc_page_zeroed() {
                            let (table_frames, installed) = unsafe {
                                akuma_exec::mmu::map_user_page(page_va, page_frame.addr, map_flags)
                            };
                            if installed {
                                if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                                    owner.address_space.track_user_frame(page_frame);
                                    for tf in table_frames {
                                        owner.address_space.track_page_table_frame(tf);
                                    }
                                } else {
                                    crate::pmm::free_page(page_frame);
                                    for tf in table_frames { crate::pmm::free_page(tf); }
                                }
                                crate::syscall::syscall_counters::inc_pagefault(1);
                                if crate::config::PROCESS_SYSCALL_STATS {
                                    if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                                        owner.syscall_stats.inc_pagefault(1);
                                    }
                                }
                            } else {
                                // Race: another CPU mapped this page. Free our frame and continue.
                                crate::pmm::free_page(page_frame);
                                if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                                    for tf in table_frames {
                                        owner.address_space.track_page_table_frame(tf);
                                    }
                                } else {
                                    for tf in table_frames { crate::pmm::free_page(tf); }
                                }
                            }
                            // Page is mapped (by us or another CPU) - success
                            return unsafe { (*frame).x0 };
                        } else {
                            let (_, _, free) = crate::pmm::stats();
                            crate::tprint!(128, "[DA-DP] pid={} va=0x{:x} anon alloc failed, {} free pages\n",
                                pid, far_usize, free);
                        }
                    }
                    } // end else (not PROT_NONE)
                } else {
                    // Fallback: check eager mmap regions — the PTE may have been lost.
                    // Use lookup_process(pid) where pid = address-space owner (from
                    // read_current_pid / process info page).  current_process() goes
                    // through THREAD_PID_MAP and returns the *worker* thread's Process
                    // for CLONE_VM threads — that Process has empty mmap_regions because
                    // all mmaps were performed on the parent.
                    let page_va = far_usize & !0xFFF;
                    let mut recovered = false;
                    if let Some(proc) = akuma_exec::process::lookup_process(pid) {
                        for (start, frames) in &proc.mmap_regions {
                            let region_end = *start + frames.len() * 4096;
                            if page_va >= *start && page_va < region_end {
                                let page_idx = (page_va - *start) / 4096;
                                let phys = frames[page_idx];
                                crate::tprint!(192, "[DP-eager] pid={} re-map va=0x{:x} frame=0x{:x}\n",
                                    pid, page_va, phys.addr);
                                let (table_frames, _) = unsafe {
                                    akuma_exec::mmu::map_user_page(page_va, phys.addr, akuma_exec::mmu::user_flags::RW_NO_EXEC)
                                };
                                for tf in table_frames {
                                    proc.address_space.track_page_table_frame(tf);
                                }
                                recovered = true;
                                break;
                            }
                        }
                    }
                    if recovered {
                        return unsafe { (*frame).x0 };
                    }
                    // Dump mmap_regions for debugging: shows what the eager fallback searched
                    if let Some(dbg_proc) = akuma_exec::process::lookup_process(pid) {
                        let n = dbg_proc.mmap_regions.len();
                        crate::tprint!(128, "[DP] eager miss: pid={} va=0x{:x} checked {} mmap_regions\n",
                            pid, far_usize, n);
                        for (i, (start, fr)) in dbg_proc.mmap_regions.iter().enumerate() {
                            if i >= 10 { crate::safe_print!(32, "  ...\n"); break; }
                            crate::safe_print!(128, "  [{}] 0x{:x}-0x{:x} ({} pages)\n",
                                i, start, start + fr.len() * 4096, fr.len());
                        }
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
                    if far_signed >= -200 && far_signed < 0 {
                        let errno = -far_signed;
                        let errno_name = match errno {
                            1 => "EPERM", 2 => "ENOENT", 3 => "ESRCH", 4 => "EINTR",
                            9 => "EBADF", 11 => "EAGAIN", 12 => "ENOMEM", 13 => "EACCES",
                            14 => "EFAULT", 17 => "EEXIST", 19 => "ENODEV", 20 => "ENOTDIR",
                            21 => "EISDIR", 22 => "EINVAL", 28 => "ENOSPC", 38 => "ENOSYS",
                            95 => "ENOTSUP", 97 => "EAFNOSUPPORT", 110 => "ETIMEDOUT",
                            115 => "EINPROGRESS",
                            _ => "???",
                        };
                        crate::tprint!(256, "[WILD-DA] *** FAR={:#x} is -{} ({}) - syscall error used as pointer! ***\n",
                            far, errno, errno_name);
                        crate::tprint!(128, "[WILD-DA] This means a syscall returned error -{} and userspace used it as a pointer\n", errno);
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

                    // Auto-dump syscall log for post-crash diagnosis.
                    // Note: CLONE_VM threads share the address space owner's process info
                    // page, so read_current_pid() returns the owner PID for all siblings —
                    // the syscall log is stored under that owner PID, not the thread's own PID.
                    match crate::syscall::log::get_formatted(pid) {
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
            if try_deliver_signal(frame, 11, far, true) {
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
            if let Some(proc) = akuma_exec::process::current_process() {
                let elapsed_us = (akuma_exec::runtime::runtime().uptime_us)()
                    .saturating_sub(proc.start_time_us);
                let secs = elapsed_us / 1_000_000;
                let frac = (elapsed_us % 1_000_000) / 10_000;
                crate::safe_print!(128, "[Fault] Process {} ({}) SIGSEGV after {}.{:02}s\n",
                    proc.pid, proc.name, secs, frac);
                crate::syscall::proc::vfork_complete(proc.pid);
            }
            akuma_exec::process::return_to_kernel(-11) // SIGSEGV - never returns
        }
        esr::EC_INST_ABORT_LOWER => {
            let far: u64;
            unsafe {
                core::arch::asm!("mrs {}, far_el1", out(reg) far);
            }
            let pid = akuma_exec::process::read_current_pid().unwrap_or(0);

            let fault_type = iss & 0x3C;
            let is_translation_fault = fault_type == 0x04 || fault_type == 0x08;
            let is_permission_fault = fault_type == 0x0C;
            let far_usize = far as usize;

            if is_permission_fault {
                if let Some((region_flags, _source, _region_start, _region_size)) = akuma_exec::process::lazy_region_lookup_for_pid(pid, far_usize) {
                    if !akuma_exec::mmu::user_flags::is_none(region_flags) {
                        let page_va = far_usize & !(0xFFF);
                        if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                            let _ = owner.address_space.update_page_flags(page_va, akuma_exec::mmu::user_flags::RX);
                            owner.address_space.invalidate_icache_for_page_va(page_va);
                            return unsafe { (*frame).x0 };
                        }
                    }
                }
            }

            if is_translation_fault {
                if let Some((flags, source, region_start, region_size)) = akuma_exec::process::lazy_region_lookup_for_pid(pid, far_usize) {
                    if akuma_exec::mmu::user_flags::is_none(flags) {
                        // PROT_NONE: don't demand-page, fall through to SIGSEGV
                    } else if far_in_kernel_identity_user_range(far) {
                        // Fault VA is in the kernel identity-map range — demand-paging
                        // would corrupt kernel memory.  Fall through to SIGSEGV.
                        crate::tprint!(128, "[IA-DP] pid={} fault in kernel VA range {:#x} -> SIGSEGV\n",
                            pid, far_usize);
                    } else {
                    let page_va = far_usize & !(0xFFF);

                    // Serialize page fault handling for this process.
                    // Insert page_va into fault_mutex so concurrent instruction faults
                    // on the same page yield until we are done mapping.
                    if let Some(proc) = akuma_exec::process::lookup_process(pid) {
                        loop {
                            {
                                let mut faults = proc.fault_mutex.lock();
                                if !faults.contains(&page_va) {
                                    faults.insert(page_va);
                                    break;
                                }
                            }
                            akuma_exec::threading::yield_now();
                        }
                    }

                    // RAII guard: remove page_va from fault_mutex on ALL exit paths
                    // from this block, including early returns and fall-through.
                    // Previously the remove happened before the mapping work, which
                    // meant the serialization window was empty.
                    struct FaultGuard { pid: u32, page_va: usize }
                    impl Drop for FaultGuard {
                        fn drop(&mut self) {
                            if let Some(proc) = akuma_exec::process::lookup_process(self.pid) {
                                proc.fault_mutex.lock().remove(&self.page_va);
                            }
                        }
                    }
                    let _fault_guard = FaultGuard { pid, page_va };

                    let map_flags = match source {
                        akuma_exec::process::LazySource::File { .. } => {
                            if flags != 0 { flags } else { akuma_exec::mmu::user_flags::RX }
                        }
                        _ => akuma_exec::mmu::user_flags::RX,
                    };


                    if let akuma_exec::process::LazySource::File { ref path, inode, file_offset, filesz, segment_va } = source {
                        crate::tprint!(256, "[IA-DP] file region: fault_va={:#x} seg_va={:#x} filesz={:#x} file_off={:#x}\n",
                            far_usize, segment_va, filesz, file_offset);
                        const READAHEAD_PAGES: usize = 256;
                        let region_end = region_start + region_size;
                        let ra_end = core::cmp::min(page_va + READAHEAD_PAGES * 0x1000, region_end);

                        // Count needed pages then batch-allocate (single PMM lock).
                        let mut needed = 0usize;
                        {
                            let mut va = page_va;
                            while va < ra_end {
                                if !akuma_exec::mmu::is_current_user_page_mapped(va) {
                                    needed += 1;
                                }
                                va += 0x1000;
                            }
                        }
                        let ia_frame_pool = if needed > 0 {
                            crate::pmm::alloc_pages_zeroed(needed).unwrap_or_else(|| {
                                let mut v = alloc::vec::Vec::new();
                                for _ in 0..needed {
                                    match crate::pmm::alloc_page_zeroed() {
                                        Some(f) => v.push(f),
                                        None => break,
                                    }
                                }
                                v
                            })
                        } else {
                            alloc::vec::Vec::new()
                        };
                        let mut ia_pool_idx = 0usize;

                        let mut any_mapped = false;
                        let mut pages_mapped = 0u64;
                        let mut cur_va = page_va;
                        while cur_va < ra_end {
                            if akuma_exec::mmu::is_current_user_page_mapped(cur_va) {
                                cur_va += 0x1000;
                                continue;
                            }
                            if ia_pool_idx >= ia_frame_pool.len() {
                                break;
                            }
                            let pf = ia_frame_pool[ia_pool_idx];
                            ia_pool_idx += 1;
                            if true {
                                let pg_data_start = core::cmp::max(cur_va, segment_va);
                                let pg_data_end = core::cmp::min(cur_va + 0x1000, segment_va + filesz);
                                if pg_data_start < pg_data_end {
                                    let dst_off = pg_data_start - cur_va;
                                    let file_off = file_offset + (pg_data_start - segment_va);
                                    let len = pg_data_end - pg_data_start;
                                    let page_ptr = akuma_exec::mmu::phys_to_virt(pf.addr);
                                    let page_buf = unsafe {
                                        core::slice::from_raw_parts_mut((page_ptr as *mut u8).add(dst_off), len)
                                    };
                                    if inode != 0 {
                                        let _ = crate::vfs::read_at_by_inode(path, inode, file_off, page_buf);
                                    } else {
                                        let _ = crate::vfs::read_at(path, file_off, page_buf);
                                    }
                                }

                                let kva = akuma_exec::mmu::phys_to_virt(pf.addr) as usize;
                                for off in (0..0x1000_usize).step_by(64) {
                                    unsafe { core::arch::asm!("dc cvau, {}", in(reg) kva + off); }
                                }
                                unsafe { core::arch::asm!("dsb ish"); }
                                for off in (0..0x1000_usize).step_by(64) {
                                    unsafe { core::arch::asm!("ic ivau, {}", in(reg) cur_va + off); }
                                }

                                // Use no_flush — TLB batched after the loop.
                                let (table_frames, installed) = unsafe {
                                    akuma_exec::mmu::map_user_page_no_flush(cur_va, pf.addr, map_flags)
                                };
                                if installed {
                                    if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                                        owner.address_space.track_user_frame(pf);
                                        for tf in table_frames {
                                            owner.address_space.track_page_table_frame(tf);
                                        }
                                    } else {
                                        crate::pmm::free_page(pf);
                                        for tf in table_frames { crate::pmm::free_page(tf); }
                                    }
                                    any_mapped = true;
                                    pages_mapped += 1;
                                } else {
                                    // Race: another CPU mapped this page between our check and
                                    // the atomic install. The page IS mapped now - don't SIGSEGV!
                                    // We just need to free our unused page and track table frames.
                                    crate::pmm::free_page(pf);
                                    if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                                        for tf in table_frames {
                                            owner.address_space.track_page_table_frame(tf);
                                        }
                                    } else {
                                        for tf in table_frames { crate::pmm::free_page(tf); }
                                    }
                                    // Critical: if this is the faulting page, it's now mapped!
                                    if cur_va == page_va {
                                        any_mapped = true;
                                    }
                                }
                            } // end if true (batch pool entry)
                            cur_va += 0x1000;
                        }

                        // Return unused frames from the batch pool back to PMM.
                        while ia_pool_idx < ia_frame_pool.len() {
                            crate::pmm::free_page(ia_frame_pool[ia_pool_idx]);
                            ia_pool_idx += 1;
                        }

                        // Single TLB flush for the entire readahead window.
                        if any_mapped {
                            akuma_exec::mmu::flush_tlb_range(page_va, pages_mapped as usize);
                        }

                        unsafe {
                            core::arch::asm!("dsb ish");
                            core::arch::asm!("isb");
                        }

                        if any_mapped {
                            crate::syscall::syscall_counters::inc_pagefault(pages_mapped);
                            if crate::config::PROCESS_SYSCALL_STATS {
                                if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                                    owner.syscall_stats.inc_pagefault(pages_mapped);
                                }
                            }
                            return unsafe { (*frame).x0 };
                        } else if akuma_exec::mmu::is_current_user_page_mapped(page_va) {
                            // Race: another CPU mapped the faulting page — return success.
                            return unsafe { (*frame).x0 };
                        } else {
                            // Readahead pool exhausted before reaching page_va.
                            // Fall back to a single-page allocation for just the faulting page.
                            let (_, _, free) = crate::pmm::stats();
                            crate::tprint!(128, "[IA-DP] pid={} va=0x{:x} readahead pool exhausted, {} free pages — retrying single page\n",
                                pid, far_usize, free);
                            if let Some(pf) = crate::pmm::alloc_page_zeroed() {
                                let pg_data_start = core::cmp::max(page_va, segment_va);
                                let pg_data_end = core::cmp::min(page_va + 0x1000, segment_va + filesz);
                                if pg_data_start < pg_data_end {
                                    let dst_off = pg_data_start - page_va;
                                    let file_off = file_offset + (pg_data_start - segment_va);
                                    let len = pg_data_end - pg_data_start;
                                    let page_ptr = akuma_exec::mmu::phys_to_virt(pf.addr);
                                    let page_buf = unsafe {
                                        core::slice::from_raw_parts_mut((page_ptr as *mut u8).add(dst_off), len)
                                    };
                                    if inode != 0 {
                                        let _ = crate::vfs::read_at_by_inode(path, inode, file_off, page_buf);
                                    } else {
                                        let _ = crate::vfs::read_at(path, file_off, page_buf);
                                    }
                                }
                                let kva = akuma_exec::mmu::phys_to_virt(pf.addr) as usize;
                                for off in (0..0x1000_usize).step_by(64) {
                                    unsafe { core::arch::asm!("dc cvau, {}", in(reg) kva + off); }
                                }
                                unsafe { core::arch::asm!("dsb ish"); }
                                for off in (0..0x1000_usize).step_by(64) {
                                    unsafe { core::arch::asm!("ic ivau, {}", in(reg) page_va + off); }
                                }
                                let (table_frames, installed) = unsafe {
                                    akuma_exec::mmu::map_user_page(page_va, pf.addr, map_flags)
                                };
                                if installed {
                                    if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                                        owner.address_space.track_user_frame(pf);
                                        for tf in table_frames { owner.address_space.track_page_table_frame(tf); }
                                    } else {
                                        crate::pmm::free_page(pf);
                                        for tf in table_frames { crate::pmm::free_page(tf); }
                                    }
                                } else {
                                    crate::pmm::free_page(pf);
                                    if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                                        for tf in table_frames { owner.address_space.track_page_table_frame(tf); }
                                    } else {
                                        for tf in table_frames { crate::pmm::free_page(tf); }
                                    }
                                }
                                unsafe { core::arch::asm!("dsb ish"); core::arch::asm!("isb"); }
                                crate::syscall::syscall_counters::inc_pagefault(1);
                                return unsafe { (*frame).x0 };
                            } else {
                                let (_, _, free2) = crate::pmm::stats();
                                crate::tprint!(128, "[IA-DP] pid={} va=0x{:x} single-page fallback OOM, {} free pages\n",
                                    pid, far_usize, free2);
                            }
                        }
                    } else {
                        if let Some(page_frame) = crate::pmm::alloc_page_zeroed() {
                            let (table_frames, installed) = unsafe {
                                akuma_exec::mmu::map_user_page(page_va, page_frame.addr, map_flags)
                            };
                            if installed {
                                if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                                    owner.address_space.track_user_frame(page_frame);
                                    for tf in table_frames {
                                        owner.address_space.track_page_table_frame(tf);
                                    }
                                } else {
                                    crate::pmm::free_page(page_frame);
                                    for tf in table_frames { crate::pmm::free_page(tf); }
                                }
                                crate::syscall::syscall_counters::inc_pagefault(1);
                                if crate::config::PROCESS_SYSCALL_STATS {
                                    if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                                        owner.syscall_stats.inc_pagefault(1);
                                    }
                                }
                            } else {
                                // Race: another CPU mapped this page. Free our frame and continue.
                                crate::pmm::free_page(page_frame);
                                if let Some(owner) = akuma_exec::process::lookup_process(pid) {
                                    for tf in table_frames {
                                        owner.address_space.track_page_table_frame(tf);
                                    }
                                } else {
                                    for tf in table_frames { crate::pmm::free_page(tf); }
                                }
                            }
                            // Page is mapped (by us or another CPU) - success
                            return unsafe { (*frame).x0 };
                        } else {
                            let (_, _, free) = crate::pmm::stats();
                            crate::tprint!(128, "[IA-DP] pid={} va=0x{:x} anon alloc failed, {} free pages\n",
                                pid, far_usize, free);
                        }
                    }
                    } // end else (not PROT_NONE)
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
            if try_deliver_signal(frame, 11, far, true) {
                return 11;
            }

            crate::safe_print!(128, "[IA] pid={} far={:#x} iss={:#x}\n", pid, far, iss);
            let frame_ref = unsafe { &*frame };
            crate::tprint!(128, "[Fault] Instruction abort from EL0 at FAR={:#x}, ISS={:#x}\n",
                far, iss);
            crate::safe_print!(128, "[Fault]  x0={:#x} x1={:#x} x2={:#x} x3={:#x}\n",
                frame_ref.x0, frame_ref.x1, frame_ref.x2, frame_ref.x3);
            crate::safe_print!(128, "[Fault]  x19={:#x} x20={:#x} x29={:#x} x30={:#x}\n",
                frame_ref.x19, frame_ref.x20, frame_ref.x29, frame_ref.x30);
            crate::safe_print!(128, "[Fault]  SP_EL0={:#x} ELR={:#x} SPSR={:#x}\n",
                frame_ref.sp_el0, frame_ref.elr_el1, frame_ref.spsr_el1);
            if let Some(proc) = akuma_exec::process::current_process() {
                let elapsed_us = (akuma_exec::runtime::runtime().uptime_us)()
                    .saturating_sub(proc.start_time_us);
                let secs = elapsed_us / 1_000_000;
                let frac = (elapsed_us % 1_000_000) / 10_000;
                crate::safe_print!(128, "[Fault] Process {} ({}) SIGSEGV after {}.{:02}s\n",
                    proc.pid, proc.name, secs, frac);
                crate::syscall::proc::vfork_complete(proc.pid);
            }
            akuma_exec::process::return_to_kernel(-11) // never returns
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

            if direction == 1 && rt < 31 {
                // MRS (read) — emulate system register reads
                let value = if op0 == 3 && op1 == 3 && crn == 0 && crm == 0 && op2 == 1 {
                    // CTR_EL0
                    let ctr: u64;
                    unsafe { core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr); }
                    ctr
                } else {
                    0
                };
                unsafe {
                    let regs = frame as *mut u64;
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
                    if op1 == 3 && crm == 11 && op2 == 1 {
                        unsafe { core::arch::asm!("dc cvau, {}", in(reg) addr); }
                    } else if op1 == 3 && crm == 5 && op2 == 1 {
                        unsafe { core::arch::asm!("ic ivau, {}", in(reg) addr); }
                    }
                }
            }
            // Advance past the trapped instruction (always 4 bytes on AArch64)
            unsafe {
                let elr_ptr = &mut (*frame).elr_el1 as *mut u64;
                let elr = core::ptr::read_volatile(elr_ptr);
                core::ptr::write_volatile(elr_ptr, elr + 4);
            }
            return unsafe { (*frame).x0 };
        }
        esr::EC_BRK_AARCH64 => {
            // BRK instruction — intentional trap/abort from user code
            if let Some(pid) = akuma_exec::process::read_current_pid() {
                crate::syscall::proc::vfork_complete(pid);
            }
            akuma_exec::process::return_to_kernel(-5) // SIGTRAP
        }
        _ => {
            // Capture additional state for debugging
            let elr: u64;
            let far: u64;
            let spsr: u64;
            let ttbr0: u64;
            let sp: u64;
            unsafe {
                core::arch::asm!("mrs {}, elr_el1", out(reg) elr);
                core::arch::asm!("mrs {}, far_el1", out(reg) far);
                core::arch::asm!("mrs {}, spsr_el1", out(reg) spsr);
                core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr0);
                core::arch::asm!("mov {}, sp", out(reg) sp);
            }
            let tid = akuma_exec::threading::current_thread_id();

            crate::safe_print!(96, "[Exception] Unknown from EL0: EC={:#x}, ISS={:#x}\n", ec, iss);
            crate::safe_print!(128, "  Thread={}, ELR={:#x}, FAR={:#x}, SPSR={:#x}\n", tid, elr, far, spsr);
            crate::safe_print!(64, "  TTBR0={:#x}, SP={:#x}\n", ttbr0, sp);

            // Check if this looks like a kernel TTBR0 (boot page tables)
            // Boot TTBR0 is typically around 0x43xxxxxx
            if ttbr0 & 0xFFFF_0000_0000_0000 == 0 && ttbr0 < 0x4400_0000 && ttbr0 > 0x4300_0000 {
                safe_print!(128, "  WARNING: TTBR0 looks like boot page tables, not user process!\n");
            }

            if let Some(pid) = akuma_exec::process::read_current_pid() {
                crate::syscall::proc::vfork_complete(pid);
            }
            akuma_exec::process::return_to_kernel(-1) // never returns
        }
    }
}
