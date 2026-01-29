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
sync_el0_handler:
    // At this point: SP = SP_EL1 (exception stack, set in run_user_until_exit)
    // ELR_EL1 = user PC, SPSR_EL1 = user PSTATE
    // CRITICAL: We must save ALL user registers we're about to clobber!
    
    // First, allocate space for the ENTIRE trap frame at once to avoid overlap issues.
    // Trap frame: 280 bytes for user regs + 16 bytes for saved kernel SP = 296 bytes
    sub     sp, sp, #296
    
    // Now save user x8-x11 first (we'll clobber these for stack operations)
    stp     x8, x9, [sp, #64]       // x8, x9 at offset 64, 72
    stp     x10, x11, [sp, #80]     // x10, x11 at offset 80, 88
    
    // Save kernel SP (for return_to_kernel) at the top of our frame
    // Kernel SP = current SP + 296 (the value before we allocated)
    add     x9, sp, #296
    str     x9, [sp, #280]          // Saved at offset 280
    
    // Save x0-x7 (not clobbered)
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
    
    // Padding at [sp, #272], kernel SP at [sp, #280]
    
    // Pass pointer to saved context as first arg
    mov     x0, sp
    
    // Enable IRQs during syscall handling to allow preemption
    // This is critical for schedule_blocking to work - timer IRQs must fire
    msr     daifclr, #2
    isb
    
    // Call Rust handler - returns syscall result in x0
    bl      rust_sync_el0_handler
    
    // Disable IRQs before restoring registers
    msr     daifset, #2
    isb
    
    // x0 now has the syscall return value
    // Save it to scratch area while we restore other registers
    str     x0, [sp, #272]
    
    // Restore SPSR_EL1 (clear IL bit to prevent EC=0xe)
    ldr     x0, [sp, #264]
    bic     x0, x0, #0x100000       // Clear IL bit (bit 20)
    msr     spsr_el1, x0
    
    // Restore ELR_EL1
    ldr     x0, [sp, #256]
    msr     elr_el1, x0
    
    // Restore SP_EL0
    ldr     x0, [sp, #248]
    msr     sp_el0, x0
    
    // Restore x30
    ldr     x30, [sp, #240]
    
    // Restore x28-x29
    ldp     x28, x29, [sp, #224]
    
    // Restore x26-x27
    ldp     x26, x27, [sp, #208]
    
    // Restore x24-x25
    ldp     x24, x25, [sp, #192]
    
    // Restore x22-x23
    ldp     x22, x23, [sp, #176]
    
    // Restore x20-x21
    ldp     x20, x21, [sp, #160]
    
    // Restore x18-x19
    ldp     x18, x19, [sp, #144]
    
    // Restore x16-x17
    ldp     x16, x17, [sp, #128]
    
    // Restore x14-x15
    ldp     x14, x15, [sp, #112]
    
    // Restore x12-x13
    ldp     x12, x13, [sp, #96]
    
    // Restore x10-x11
    ldp     x10, x11, [sp, #80]
    
    // Restore x8-x9
    ldp     x8, x9, [sp, #64]
    
    // Restore x6-x7
    ldp     x6, x7, [sp, #48]
    
    // Restore x4-x5
    ldp     x4, x5, [sp, #32]
    
    // Restore x2-x3
    ldp     x2, x3, [sp, #16]
    
    // Restore x1
    ldr     x1, [sp, #8]
    
    // Load syscall return value into x0
    ldr     x0, [sp, #272]
    
    // Cleanup stack frame (296 bytes)
    add     sp, sp, #296
    
    // Return to user mode
    eret

// IRQ from EL0 (user mode)
// UNIFIED: Stack-based save/restore, same mechanism as EL1 IRQ handler.
// Context switch: Rust handler returns new SP, assembly does the actual switch.
// 
// EL0 IRQ frame layout (288 bytes total):
//   [sp+0]:   x30 + padding
//   [sp+16]:  x28, x29
//   [sp+32]:  x26, x27
//   [sp+48]:  x24, x25
//   [sp+64]:  x22, x23
//   [sp+80]:  x20, x21
//   [sp+96]:  x18, x19
//   [sp+112]: x16, x17
//   [sp+128]: x14, x15
//   [sp+144]: x12, x13
//   [sp+160]: x8, x9
//   [sp+176]: x6, x7
//   [sp+192]: x4, x5
//   [sp+208]: x2, x3
//   [sp+224]: x0, x1
//   [sp+240]: ELR_EL1, SPSR_EL1
//   [sp+256]: SP_EL0 + padding
//   [sp+272]: x10, x11
irq_el0_handler:
    // ============================================================
    // SAVE PHASE: Push all registers to stack in fixed layout
    // EL0 IRQ frame: 288 bytes (includes SP_EL0)
    // ============================================================
    
    // First save x10, x11 (need them for ELR/SPSR/SP_EL0)
    stp     x10, x11, [sp, #-16]!
    
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
    
    // Restore original x10, x11
    ldp     x10, x11, [sp], #16
    
    eret

// IRQ from EL1 (kernel mode)
// UNIFIED: Stack-based save/restore, same frame layout as EL0 IRQ handler.
// Context switch: Rust handler returns new SP, assembly does the actual switch.
//
// UNIFIED IRQ frame layout (288 bytes total) - same as EL0:
//   [sp+0]:   x30 + padding
//   [sp+16]:  x28, x29
//   [sp+32]:  x26, x27
//   [sp+48]:  x24, x25
//   [sp+64]:  x22, x23
//   [sp+80]:  x20, x21
//   [sp+96]:  x18, x19
//   [sp+112]: x16, x17
//   [sp+128]: x14, x15
//   [sp+144]: x12, x13
//   [sp+160]: x8, x9
//   [sp+176]: x6, x7
//   [sp+192]: x4, x5
//   [sp+208]: x2, x3
//   [sp+224]: x0, x1
//   [sp+240]: ELR_EL1, SPSR_EL1
//   [sp+256]: SP_EL0 + padding (preserved for user stack during syscalls)
//   [sp+272]: x10, x11
irq_handler:
    // ============================================================
    // SAVE PHASE: Push all registers to stack in fixed layout
    // IRQ frame: 288 bytes total (unified with EL0 handler)
    // ============================================================
    
    // First save x10, x11 (need them for ELR/SPSR/SP_EL0)
    stp     x10, x11, [sp, #-16]!
    
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
    // Boot thread (thread 0) uses the boot stack at 0x42000000
    // Its exception stack is at the very top
    let boot_stack_top = 0x42000000u64;
    set_current_exception_stack(boot_stack_top);
}

/// Saved user context from EL0 exception
/// Layout must match the assembly save/restore sequence
#[repr(C)]
pub struct UserTrapFrame {
    pub x0: u64,
    pub x1: u64,
    pub x2: u64,
    pub x3: u64,
    pub x4: u64,
    pub x5: u64,
    pub x6: u64,
    pub x7: u64,
    pub x8: u64, // Syscall number
    pub x9: u64,
    pub x10: u64,
    pub x11: u64,
    pub x12: u64,
    pub x13: u64,
    pub x14: u64,
    pub x15: u64,
    pub x16: u64,
    pub x17: u64,
    pub x18: u64,
    pub x19: u64,
    pub x20: u64,
    pub x21: u64,
    pub x22: u64,
    pub x23: u64,
    pub x24: u64,
    pub x25: u64,
    pub x26: u64,
    pub x27: u64,
    pub x28: u64,
    pub x29: u64,
    pub x30: u64,
    pub sp_el0: u64,
    pub elr_el1: u64, // User PC
    pub spsr_el1: u64,
    pub _padding: u64,
}

/// ESR_EL1 exception class values
mod esr {
    pub const EC_SVC64: u64 = 0b010101; // SVC instruction from AArch64
    pub const EC_DATA_ABORT_LOWER: u64 = 0b100100; // Data abort from lower EL
    pub const EC_INST_ABORT_LOWER: u64 = 0b100000; // Instruction abort from lower EL
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
    let tid = crate::threading::current_thread_id();
    
    // Use stack-only print to avoid heap allocation in exception context
    crate::safe_print!(128, "[Exception] Default handler: EC={:#x}, ELR={:#x}, SPSR={:#x}\n",
        ec, elr, spsr);
    crate::safe_print!(96, "  Thread={}, TTBR0={:#x}, SP={:#x}\n", tid, ttbr0, sp);
    
    // Check for dangerous ERET conditions
    let target_el = spsr & 0xF;
    if target_el == 0 {
        crate::console::print("  WARNING: SPSR indicates EL0 - ERET would go to user mode!\n");
    }
    if elr == 0 {
        crate::console::print("  WARNING: ELR=0 - ERET would jump to address 0!\n");
    }
    if elr < 0x4000_0000 && target_el != 0 {
        crate::safe_print!(96, "  WARNING: ELR={:#x} looks like user address but SPSR is EL1!\n", elr);
    }
    
    // If ERET would be dangerous, halt instead of returning
    if elr == 0 || (target_el == 0 && elr < 0x4000_0000) {
        crate::console::print("  HALTING to prevent invalid ERET\n");
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
            return crate::threading::sgi_scheduler_handler_with_sp(irq, current_sp);
        } else {
            // Normal IRQs: call handler then EOI
            crate::irq::dispatch_irq(irq);
            crate::gic::end_of_interrupt(irq);
        }
    }
    0  // No context switch
}

/// Synchronous exception handler from EL1 (kernel mode)
/// Uses static buffers to avoid heap allocation during crash
#[unsafe(no_mangle)]
extern "C" fn rust_sync_el1_handler() {
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
    let tid = crate::threading::current_thread_id();

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
        crate::console::print("  WARNING: Kernel accessing user-space address!\n");
        crate::console::print("  This suggests stale TTBR0 or dereferencing user pointer from kernel.\n");
    }
    
    // Check for page table corruption on translation table walk faults
    let dfsc = iss & 0x3F;
    if dfsc == 0x21 || dfsc == 0x22 || dfsc == 0x23 {
        // External abort on translation table walk (level 1/2/3)
        crate::console::print("  PAGE TABLE WALK FAULT - checking page table integrity:\n");
        
        // Get expected boot TTBR0
        let boot_ttbr0 = crate::mmu::get_boot_ttbr0();
        let _ = write!(w, "    Expected boot_ttbr0: {:#x}\n", boot_ttbr0);
        w.flush();
        let _ = write!(w, "    Current TTBR0:       {:#x}\n", ttbr0);
        w.flush();
        
        if ttbr0 != boot_ttbr0 {
            crate::console::print("    WARNING: TTBR0 mismatch!\n");
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
            crate::console::print("    WARNING: L1 address mismatch - page table corrupted!\n");
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
                crate::console::print("    WARNING: L1[0] is INVALID!\n");
            }
            if block_addr != 0 {
                crate::console::print("    WARNING: L1[0] block address wrong!\n");
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
    
    crate::console::print("\n=== Memory Stats at Crash ===\n");
    
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
    let (thread_count, running, terminated) = crate::threading::thread_stats();
    let _ = write!(w, "  Threads: {} total, {} running, {} terminated\n",
        thread_count, running, terminated
    );
    w.flush();
    
    // Current thread's kernel stack info
    if let Some(stack_info) = crate::threading::get_thread_stack_info(tid) {
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
            crate::console::print("  WARNING: Kernel SP outside thread's stack bounds!\n");
        }
    }
    
    // User process info (if any)
    if let Some(proc) = crate::process::current_process() {
        let mem = &proc.memory;
        let stack_size = mem.stack_top - mem.stack_bottom;
        let stack_used = if user_sp >= mem.stack_bottom as u64 && user_sp < mem.stack_top as u64 {
            mem.stack_top - user_sp as usize
        } else {
            0 // SP outside expected range (might be corrupted)
        };
        let heap_used = proc.brk.saturating_sub(proc.initial_brk);
        let mmap_used = mem.next_mmap.saturating_sub(0x1000_0000);
        
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
            mem.next_mmap, mem.mmap_limit, mmap_used
        );
        w.flush();
        
        if user_sp < mem.stack_bottom as u64 {
            crate::console::print("    WARNING: User SP below stack bottom - STACK OVERFLOW!\n");
        } else if user_sp >= mem.stack_top as u64 {
            crate::console::print("    WARNING: User SP above stack top - SP corrupted!\n");
        }
    } else {
        crate::console::print("  No current user process\n");
    }
    
    crate::console::print("=============================\n");
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
            
            // Check if process exited - if so, return to kernel
            if let Some(proc) = crate::process::current_process() {
                if proc.exited {
                    let exit_code = proc.exit_code;
                    
                    // Validate exit code - detect corruption (pointer-like values)
                    let exit_code_u32 = exit_code as u32;
                    if exit_code_u32 >= 0x40000000 && exit_code_u32 < 0x50000000 {
                        crate::console::print("[exception] CORRUPT EXIT CODE DETECTED!\n");
                        crate::safe_print!(128, "  PID={}, exit_code={} (0x{:x}) looks like kernel address\n",
                            proc.pid, exit_code, exit_code_u32);
                        crate::safe_print!(96, "  proc ptr=0x{:x}, &exit_code=0x{:x}\n",
                            proc as *const _ as usize, 
                            &proc.exit_code as *const _ as usize);
                        // Also check if the syscall frame x0 matches
                        let frame_x0 = unsafe { (*frame).x0 };
                        crate::safe_print!(64, "  frame.x0=0x{:x} (syscall arg)\n", frame_x0);
                    }
                    
                    // Use stack-only print to avoid heap allocation in exception context
                    crate::safe_print!(128, "[exception] Process {} ({}) exited, calling return_to_kernel({})\n",
                        proc.pid, proc.name, exit_code);
                    // Don't ERET back to user - return to kernel instead
                    crate::process::return_to_kernel(exit_code);
                }
            } else {
                // Only log if we just handled EXIT syscall
                if syscall_num == 93 {
                    crate::console::print("[exception] WARNING: EXIT syscall but no current_process!\n");
                }
            }

            ret
        }
        esr::EC_DATA_ABORT_LOWER => {
            // Data abort from user - terminate with error
            let far: u64;
            let elr: u64;
            unsafe {
                core::arch::asm!("mrs {}, far_el1", out(reg) far);
                core::arch::asm!("mrs {}, elr_el1", out(reg) elr);
            }
            crate::safe_print!(128, "[Fault] Data abort from EL0 at FAR={:#x}, ELR={:#x}, ISS={:#x}\n",
                far, elr, iss);
            // Terminate process
            crate::process::return_to_kernel(-11) // SIGSEGV - never returns
        }
        esr::EC_INST_ABORT_LOWER => {
            // Instruction abort from user - terminate
            let far: u64;
            unsafe {
                core::arch::asm!("mrs {}, far_el1", out(reg) far);
            }
            crate::safe_print!(96, "[Fault] Instruction abort from EL0 at FAR={:#x}, ISS={:#x}\n",
                far, iss);
            crate::process::return_to_kernel(-11) // never returns
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
            let tid = crate::threading::current_thread_id();
            
            crate::safe_print!(96, "[Exception] Unknown from EL0: EC={:#x}, ISS={:#x}\n", ec, iss);
            crate::safe_print!(128, "  Thread={}, ELR={:#x}, FAR={:#x}, SPSR={:#x}\n", tid, elr, far, spsr);
            crate::safe_print!(64, "  TTBR0={:#x}, SP={:#x}\n", ttbr0, sp);
            
            // Check if this looks like a kernel TTBR0 (boot page tables)
            // Boot TTBR0 is typically around 0x43xxxxxx
            if ttbr0 & 0xFFFF_0000_0000_0000 == 0 && ttbr0 < 0x4400_0000 && ttbr0 > 0x4300_0000 {
                crate::console::print("  WARNING: TTBR0 looks like boot page tables, not user process!\n");
            }
            
            crate::process::return_to_kernel(-1) // never returns
        }
    }
}
