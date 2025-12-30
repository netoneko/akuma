// ARM64 Exception handling

use core::arch::global_asm;

// Exception vector table
global_asm!(
    r#"
.section .text.exceptions
.balign 0x800

.global exception_vector_table
exception_vector_table:
    // Current EL with SP0
    .balign 0x80
    b sync_el1_handler             // Synchronous
    .balign 0x80
    b irq_handler                  // IRQ
    .balign 0x80
    b default_exception_handler   // FIQ
    .balign 0x80
    b default_exception_handler   // SError

    // Current EL with SPx
    .balign 0x80
    b sync_el1_handler             // Synchronous
    .balign 0x80
    b irq_handler                  // IRQ
    .balign 0x80
    b default_exception_handler   // FIQ
    .balign 0x80
    b default_exception_handler   // SError

    // Lower EL using AArch64 (EL0 -> EL1)
    .balign 0x80
    b sync_el0_handler             // Synchronous (includes SVC)
    .balign 0x80
    b irq_el0_handler              // IRQ
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

// Synchronous exception from EL1 - handle and return
sync_el1_handler:
    // Save minimal context
    stp x29, x30, [sp, #-16]!
    stp x0, x1, [sp, #-16]!
    
    // Call Rust handler
    bl rust_sync_el1_handler
    
    // Restore and return
    ldp x0, x1, [sp], #16
    ldp x29, x30, [sp], #16
    eret

// Synchronous exception from EL0 (user mode) - includes SVC
sync_el0_handler:
    // Switch to kernel stack (SP_EL1)
    // Save user registers to kernel stack
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
    
    // Save SP_EL0 (user stack pointer)
    mrs x9, sp_el0
    str x9, [sp, #-16]!
    
    // Save ELR_EL1 (return address)
    mrs x9, elr_el1
    str x9, [sp, #-16]!
    
    // Save SPSR_EL1
    mrs x9, spsr_el1
    str x9, [sp, #-16]!
    
    // Pass stack pointer to Rust handler (contains saved context)
    mov x0, sp
    
    // Call Rust syscall/exception handler
    bl rust_sync_el0_handler
    
    // x0 now contains the return value (for syscalls)
    // Save it temporarily
    mov x9, x0
    
    // Restore SPSR_EL1
    ldr x10, [sp], #16
    msr spsr_el1, x10
    
    // Restore ELR_EL1
    ldr x10, [sp], #16
    msr elr_el1, x10
    
    // Restore SP_EL0
    ldr x10, [sp], #16
    msr sp_el0, x10
    
    // Restore general registers
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
    ldp x8, x8, [sp], #16   // Skip x9 restore, we use it for return value
    ldp x6, x7, [sp], #16
    ldp x4, x5, [sp], #16
    ldp x2, x3, [sp], #16
    ldp x0, x1, [sp], #16
    
    // Put syscall return value in x0
    mov x0, x9
    
    eret

// IRQ from EL0 - similar save/restore
irq_el0_handler:
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

    bl rust_irq_handler

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

/// ESR_EL1 exception class values
mod esr {
    pub const EC_SVC64: u64 = 0b010101; // SVC instruction from AArch64
    pub const EC_DATA_ABORT_LOWER: u64 = 0b100100; // Data abort from lower EL
    pub const EC_INST_ABORT_LOWER: u64 = 0b100000; // Instruction abort from lower EL
}

/// Saved user context on the stack
#[repr(C)]
pub struct UserTrapFrame {
    pub spsr: u64,
    pub elr: u64,
    pub sp_el0: u64,
    pub x30: u64,
    pub x28: u64,
    pub x29: u64,
    pub x26: u64,
    pub x27: u64,
    pub x24: u64,
    pub x25: u64,
    pub x22: u64,
    pub x23: u64,
    pub x20: u64,
    pub x21: u64,
    pub x18: u64,
    pub x19: u64,
    pub x16: u64,
    pub x17: u64,
    pub x14: u64,
    pub x15: u64,
    pub x12: u64,
    pub x13: u64,
    pub x10: u64,
    pub x11: u64,
    pub x8: u64,
    pub x9: u64,
    pub x6: u64,
    pub x7: u64,
    pub x4: u64,
    pub x5: u64,
    pub x2: u64,
    pub x3: u64,
    pub x0: u64,
    pub x1: u64,
}

/// Synchronous exception handler from EL0 (user mode)
/// Returns the syscall return value
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
            // System call
            // Syscall number is in x8, arguments in x0-x5
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
            crate::syscall::handle_syscall(syscall_num, &args)
        }
        esr::EC_DATA_ABORT_LOWER => {
            // Data abort from user - likely a segfault
            let far: u64;
            unsafe {
                core::arch::asm!("mrs {}, far_el1", out(reg) far);
            }
            crate::console::print(&alloc::format!(
                "[Exception] Data abort from EL0 at FAR={:#x}, ISS={:#x}\n",
                far, iss
            ));
            // Kill the process (return -1 as if exit syscall)
            u64::MAX
        }
        esr::EC_INST_ABORT_LOWER => {
            // Instruction abort from user
            let far: u64;
            unsafe {
                core::arch::asm!("mrs {}, far_el1", out(reg) far);
            }
            crate::console::print(&alloc::format!(
                "[Exception] Instruction abort from EL0 at FAR={:#x}, ISS={:#x}\n",
                far, iss
            ));
            u64::MAX
        }
        _ => {
            crate::console::print(&alloc::format!(
                "[Exception] Unknown exception from EL0: EC={:#x}, ISS={:#x}\n",
                ec, iss
            ));
            u64::MAX
        }
    }
}

/// Synchronous exception handler from EL1 (kernel mode)
#[unsafe(no_mangle)]
extern "C" fn rust_sync_el1_handler() {
    // Read ESR_EL1 to determine exception type
    let esr: u64;
    let elr: u64;
    let far: u64;
    unsafe {
        core::arch::asm!("mrs {}, esr_el1", out(reg) esr);
        core::arch::asm!("mrs {}, elr_el1", out(reg) elr);
        core::arch::asm!("mrs {}, far_el1", out(reg) far);
    }

    let ec = (esr >> 26) & 0x3F;
    let iss = esr & 0x1FFFFFF;

    crate::console::print(&alloc::format!(
        "[Exception] Sync from EL1: EC={:#x}, ISS={:#x}, ELR={:#x}, FAR={:#x}\n",
        ec, iss, elr, far
    ));

    // For now, just continue (may cause issues if it's a real fault)
}
