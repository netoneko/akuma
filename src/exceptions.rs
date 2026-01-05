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
    bl      rust_default_exception_handler
    ldp     x29, x30, [sp], #16
    ldp     x0, x1, [sp], #16
    eret

// Synchronous exception from EL1 (kernel fault)
sync_el1_handler:
    // Save minimal context
    stp     x29, x30, [sp, #-16]!
    stp     x0, x1, [sp, #-16]!
    
    // Call Rust handler
    bl      rust_sync_el1_handler
    
    // Restore and return
    ldp     x0, x1, [sp], #16
    ldp     x29, x30, [sp], #16
    eret

// Synchronous exception from EL0 (user mode)
// Handles SVC syscalls and user faults
sync_el0_handler:
    // At this point: SP = SP_EL1 (kernel stack), ELR_EL1 = user PC
    
    // Switch to dedicated exception stack to avoid corrupting kernel stack
    // Save current SP to x9, then load exception stack
    mov     x9, sp
    adrp    x10, EXCEPTION_STACK_TOP
    add     x10, x10, :lo12:EXCEPTION_STACK_TOP
    ldr     x10, [x10]
    mov     sp, x10
    
    // Save original kernel SP at top of exception stack (we'll need it for return_to_kernel)
    str     x9, [sp, #-16]!
    
    // Now allocate space for user registers on exception stack
    // Order: x0-x30, then SP_EL0, ELR_EL1, SPSR_EL1
    sub     sp, sp, #280            // 35 * 8 bytes
    
    // Save x0-x30
    stp     x0, x1, [sp, #0]
    stp     x2, x3, [sp, #16]
    stp     x4, x5, [sp, #32]
    stp     x6, x7, [sp, #48]
    stp     x8, x9, [sp, #64]       // x9 was original sp, now saved
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
    
    // Save SP_EL0
    mrs     x0, sp_el0
    str     x0, [sp, #248]
    
    // Save ELR_EL1 (user PC)
    mrs     x0, elr_el1
    str     x0, [sp, #256]
    
    // Save SPSR_EL1
    mrs     x0, spsr_el1
    str     x0, [sp, #264]
    
    // Padding at [sp, #272] for alignment
    
    // Pass pointer to saved context as first arg
    mov     x0, sp
    
    // Call Rust handler - returns syscall result in x0
    bl      rust_sync_el0_handler
    
    // x0 now has the return value
    // Save it temporarily in x9 (will restore after we load other regs)
    mov     x9, x0
    
    // Restore SPSR_EL1
    ldr     x0, [sp, #264]
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
    
    // Restore x8 (skip x9, we're using it for return value)
    ldr     x8, [sp, #64]
    
    // Restore x6-x7
    ldp     x6, x7, [sp, #48]
    
    // Restore x4-x5
    ldp     x4, x5, [sp, #32]
    
    // Restore x2-x3
    ldp     x2, x3, [sp, #16]
    
    // Restore x1 (x0 will be syscall return)
    ldr     x1, [sp, #8]
    
    // Put syscall return value in x0
    mov     x0, x9
    
    // Cleanup stack - pop the user register frame
    add     sp, sp, #280
    
    // Pop the saved original kernel SP (we pushed it at entry)
    // Note: we don't actually need to restore it since we're returning to user mode
    // but we do need to clean up the exception stack
    add     sp, sp, #16
    
    // Return to user mode
    eret

// IRQ from EL0 (user mode)
irq_el0_handler:
    // Save full user context
    sub     sp, sp, #256
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

    bl      rust_irq_handler

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
    ldp     x0, x1, [sp, #0]
    add     sp, sp, #256

    eret

// IRQ from EL1 (kernel mode)
irq_handler:
    // Save caller-saved registers
    stp     x0, x1, [sp, #-16]!
    stp     x2, x3, [sp, #-16]!
    stp     x4, x5, [sp, #-16]!
    stp     x6, x7, [sp, #-16]!
    stp     x8, x9, [sp, #-16]!
    stp     x10, x11, [sp, #-16]!
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

    bl      rust_irq_handler

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
    ldp     x10, x11, [sp], #16
    ldp     x8, x9, [sp], #16
    ldp     x6, x7, [sp], #16
    ldp     x4, x5, [sp], #16
    ldp     x2, x3, [sp], #16
    ldp     x0, x1, [sp], #16

    eret
"#
);

unsafe extern "C" {
    static exception_vector_table: u8;
}

/// Exception stack size (16KB should be plenty for syscall handling)
const EXCEPTION_STACK_SIZE: usize = 16 * 1024;

/// Static exception stack - used by sync_el0_handler to avoid corrupting kernel stack
#[repr(C, align(16))]
struct ExceptionStack {
    data: [u8; EXCEPTION_STACK_SIZE],
}

static mut EXCEPTION_STACK: ExceptionStack = ExceptionStack {
    data: [0; EXCEPTION_STACK_SIZE],
};

/// Pointer to top of exception stack (stack grows down)
#[unsafe(no_mangle)]
static mut EXCEPTION_STACK_TOP: u64 = 0;

/// Initialize the exception stack pointer
/// Must be called before any user mode code runs
pub fn init_exception_stack() {
    unsafe {
        let stack_bottom = core::ptr::addr_of!(EXCEPTION_STACK.data) as u64;
        let stack_top = stack_bottom + EXCEPTION_STACK_SIZE as u64;
        // Align to 16 bytes (required by AArch64 ABI)
        EXCEPTION_STACK_TOP = stack_top & !0xF;
    }
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
#[unsafe(no_mangle)]
extern "C" fn rust_default_exception_handler() {
    let esr: u64;
    let elr: u64;
    let spsr: u64;
    unsafe {
        core::arch::asm!("mrs {}, esr_el1", out(reg) esr);
        core::arch::asm!("mrs {}, elr_el1", out(reg) elr);
        core::arch::asm!("mrs {}, spsr_el1", out(reg) spsr);
    }
    let ec = (esr >> 26) & 0x3F;
    crate::console::print(&alloc::format!(
        "[Exception] Default handler: EC={:#x}, ELR={:#x}, SPSR={:#x}\n",
        ec,
        elr,
        spsr
    ));
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

/// Synchronous exception handler from EL1 (kernel mode)
#[unsafe(no_mangle)]
extern "C" fn rust_sync_el1_handler() {
    // Read ESR_EL1 to determine exception type
    let esr: u64;
    let elr: u64;
    let far: u64;
    let spsr: u64;
    unsafe {
        core::arch::asm!("mrs {}, esr_el1", out(reg) esr);
        core::arch::asm!("mrs {}, elr_el1", out(reg) elr);
        core::arch::asm!("mrs {}, far_el1", out(reg) far);
        core::arch::asm!("mrs {}, spsr_el1", out(reg) spsr);
    }

    let ec = (esr >> 26) & 0x3F;
    let iss = esr & 0x1FFFFFF;

    crate::console::print(&alloc::format!(
        "[Exception] Sync from EL1: EC={:#x}, ISS={:#x}, ELR={:#x}, FAR={:#x}, SPSR={:#x}\n",
        ec,
        iss,
        elr,
        far,
        spsr
    ));

    // Halt on kernel exceptions - they indicate bugs
    loop {
        unsafe {
            core::arch::asm!("wfe");
        }
    }
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
                    // Don't ERET back to user - return to kernel instead
                    crate::process::return_to_kernel(proc.exit_code);
                }
            }

            ret
        }
        esr::EC_DATA_ABORT_LOWER => {
            // Data abort from user - terminate with error
            let far: u64;
            unsafe {
                core::arch::asm!("mrs {}, far_el1", out(reg) far);
            }
            crate::console::print(&alloc::format!(
                "[Fault] Data abort from EL0 at FAR={:#x}, ISS={:#x}\n",
                far,
                iss
            ));
            // Terminate process
            crate::process::return_to_kernel(-11) // SIGSEGV - never returns
        }
        esr::EC_INST_ABORT_LOWER => {
            // Instruction abort from user - terminate
            let far: u64;
            unsafe {
                core::arch::asm!("mrs {}, far_el1", out(reg) far);
            }
            crate::console::print(&alloc::format!(
                "[Fault] Instruction abort from EL0 at FAR={:#x}, ISS={:#x}\n",
                far,
                iss
            ));
            crate::process::return_to_kernel(-11) // never returns
        }
        _ => {
            crate::console::print(&alloc::format!(
                "[Exception] Unknown from EL0: EC={:#x}, ISS={:#x}\n",
                ec,
                iss
            ));
            crate::process::return_to_kernel(-1) // never returns
        }
    }
}
