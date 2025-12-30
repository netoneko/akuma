// ARM64 Exception handling

use core::arch::global_asm;

// Exception vector table - Reverted to simple handlers for stability
global_asm!(
    r#"
.section .text.exceptions
.balign 0x800

.global exception_vector_table
exception_vector_table:
    // Current EL with SP0
    .balign 0x80
    b default_exception_handler   // Synchronous
    .balign 0x80
    b irq_handler                  // IRQ
    .balign 0x80
    b default_exception_handler   // FIQ
    .balign 0x80
    b default_exception_handler   // SError

    // Current EL with SPx
    .balign 0x80
    b default_exception_handler   // Synchronous
    .balign 0x80
    b irq_handler                  // IRQ
    .balign 0x80
    b default_exception_handler   // FIQ
    .balign 0x80
    b default_exception_handler   // SError

    // Lower EL using AArch64 (EL0 -> EL1)
    .balign 0x80
    b default_exception_handler   // Synchronous - will add SVC handling later
    .balign 0x80
    b irq_handler                  // IRQ
    .balign 0x80
    b default_exception_handler   // FIQ
    .balign 0x80
    b default_exception_handler   // SError

    // Lower EL using AArch32
    .balign 0x80
    b default_exception_handler   // Synchronous
    .balign 0x80
    b irq_handler                  // IRQ
    .balign 0x80
    b default_exception_handler   // FIQ
    .balign 0x80
    b default_exception_handler   // SError

// Default exception handler - just returns
default_exception_handler:
    eret

// IRQ handler - saves context and calls Rust handler
irq_handler:
    // Save all registers
    stp x0, x1, [sp, #-16]!
    stp x2, x3, [sp, #-16]!
    stp x4, x5, [sp, #-16]!
    stp x6, x7, [sp, #-16]!
    stp x8, x9, [sp, #-16]!
    stp x10, x11, [sp, #-16]!
    stp x12, x13, [sp, #-16]!
    stp x14, x15, [sp, #-16]!
    stp x16, x17, [sp, #-16]!
    stp x18, x19, [sp, #-16]!
    stp x20, x21, [sp, #-16]!
    stp x22, x23, [sp, #-16]!
    stp x24, x25, [sp, #-16]!
    stp x26, x27, [sp, #-16]!
    stp x28, x29, [sp, #-16]!
    str x30, [sp, #-16]!

    // Call Rust IRQ handler
    bl rust_irq_handler

    // Restore all registers
    ldr x30, [sp], #16
    ldp x28, x29, [sp], #16
    ldp x26, x27, [sp], #16
    ldp x24, x25, [sp], #16
    ldp x22, x23, [sp], #16
    ldp x20, x21, [sp], #16
    ldp x18, x19, [sp], #16
    ldp x16, x17, [sp], #16
    ldp x14, x15, [sp], #16
    ldp x12, x13, [sp], #16
    ldp x10, x11, [sp], #16
    ldp x8, x9, [sp], #16
    ldp x6, x7, [sp], #16
    ldp x4, x5, [sp], #16
    ldp x2, x3, [sp], #16
    ldp x0, x1, [sp], #16

    eret
"#
);

unsafe extern "C" {
    static exception_vector_table: u8;
}

/// Install exception vector table
pub fn init() {
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

/// Rust IRQ handler called from assembly
#[unsafe(no_mangle)]
extern "C" fn rust_irq_handler() {
    // Acknowledge the interrupt and get IRQ number
    if let Some(irq) = crate::gic::acknowledge_irq() {
        // Special handling for scheduler SGI
        if irq == crate::gic::SGI_SCHEDULER {
            // SGI handler calls EOI itself before context switching
            crate::threading::sgi_scheduler_handler(irq);
        } else {
            // Normal IRQs: call handler then EOI
            crate::irq::dispatch_irq(irq);
            crate::gic::end_of_interrupt(irq);
        }
    }
}
