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
// Uses a FIXED location relative to TPIDR_EL1, calculated fresh on both entry and exit.
// Frame is at [tpidr_el1 - 768] to avoid overlap with sync_el0_handler.
irq_el0_handler:
    // At entry: all x0-x30 contain USER values, SP = SP_EL1
    // Must save x10 BEFORE we clobber it for frame address calculation
    
    // Step 1: Push x10 to SP stack temporarily (SP = SP_EL1, which is exception stack)
    str     x10, [sp, #-16]!
    
    // Step 2: Calculate frame address in x10
    mrs     x10, tpidr_el1
    sub     x10, x10, #768          // x10 = frame base
    
    // Step 3: Save x11 to frame
    str     x11, [x10, #88]
    
    // Step 4: Retrieve saved x10 from stack and save to frame
    ldr     x11, [sp], #16          // Pop saved x10 into x11, restore SP
    str     x11, [x10, #80]         // Save user x10 to its slot
    
    // Step 5: Save x8, x9
    stp     x8, x9, [x10, #64]
    
    // Step 6: Save all other registers
    stp     x0, x1, [x10, #0]
    stp     x2, x3, [x10, #16]
    stp     x4, x5, [x10, #32]
    stp     x6, x7, [x10, #48]
    // x8-x11 already saved above
    stp     x12, x13, [x10, #96]
    stp     x14, x15, [x10, #112]
    stp     x16, x17, [x10, #128]
    stp     x18, x19, [x10, #144]
    stp     x20, x21, [x10, #160]
    stp     x22, x23, [x10, #176]
    stp     x24, x25, [x10, #192]
    stp     x26, x27, [x10, #208]
    stp     x28, x29, [x10, #224]
    str     x30, [x10, #240]
    
    // Save ELR_EL1 and SPSR_EL1
    mrs     x0, elr_el1
    mrs     x1, spsr_el1
    stp     x0, x1, [x10, #248]
    
    // Set SP for function calls (below our frame)
    sub     sp, x10, #256

    bl      rust_irq_handler

    // After return (possibly from different thread via context switch),
    // recalculate frame address from TPIDR_EL1 (which was updated by scheduler)
    mrs     x10, tpidr_el1
    sub     x10, x10, #768
    
    // Restore ELR_EL1 and SPSR_EL1
    ldp     x0, x1, [x10, #248]
    msr     elr_el1, x0
    msr     spsr_el1, x1
    
    ldr     x30, [x10, #240]
    ldp     x28, x29, [x10, #224]
    ldp     x26, x27, [x10, #208]
    ldp     x24, x25, [x10, #192]
    ldp     x22, x23, [x10, #176]
    ldp     x20, x21, [x10, #160]
    ldp     x18, x19, [x10, #144]
    ldp     x16, x17, [x10, #128]
    ldp     x14, x15, [x10, #112]
    ldp     x12, x13, [x10, #96]
    ldp     x8, x9, [x10, #64]
    ldp     x6, x7, [x10, #48]
    ldp     x4, x5, [x10, #32]
    ldp     x2, x3, [x10, #16]
    ldp     x0, x1, [x10, #0]
    // Finally restore x10, x11 - load x11 first using x10 as base
    ldr     x11, [x10, #88]         // Load x11 from its slot
    ldr     x10, [x10, #80]         // Load x10 last (clobbers base)

    eret

// IRQ from EL1 (kernel mode)
// Must save ELR_EL1 and SPSR_EL1 because context switch may modify them
irq_handler:
    // Save x10, x11 FIRST (we need them as scratch for ELR/SPSR)
    stp     x10, x11, [sp, #-16]!
    
    // Save ELR_EL1 and SPSR_EL1 (critical for nested IRQs + context switch)
    mrs     x10, elr_el1
    mrs     x11, spsr_el1
    stp     x10, x11, [sp, #-16]!
    
    // Save remaining caller-saved registers
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
    ldp     x8, x9, [sp], #16
    ldp     x6, x7, [sp], #16
    ldp     x4, x5, [sp], #16
    ldp     x2, x3, [sp], #16
    ldp     x0, x1, [sp], #16
    
    // Restore ELR_EL1 and SPSR_EL1
    ldp     x10, x11, [sp], #16
    msr     elr_el1, x10
    // Clear IRQ mask bit before restoring SPSR (ensure IRQs enabled after ERET)
    bic     x11, x11, #0x80
    msr     spsr_el1, x11
    
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
    // Debug: read TPIDR_EL1 and SP
    let tpidr: u64;
    let sp: u64;
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el1", out(reg) tpidr);
        core::arch::asm!("mov {}, sp", out(reg) sp);
    }
    let tid_before = crate::threading::current_thread_id();
    if crate::config::ENABLE_IRQ_DEBUG_PRINTS {
        crate::console::print(&alloc::format!(
            "[IRQ] entry: tid={} tpidr={:#x} sp={:#x}\n", tid_before, tpidr, sp
        ));
    }
    
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
    
    // Debug: after handling
    let tpidr_after: u64;
    let sp_after: u64;
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el1", out(reg) tpidr_after);
        core::arch::asm!("mov {}, sp", out(reg) sp_after);
    }
    let tid_after = crate::threading::current_thread_id();
    if crate::config::ENABLE_IRQ_DEBUG_PRINTS {
        crate::console::print(&alloc::format!(
            "[IRQ] exit: tid={} tpidr={:#x} sp={:#x}\n", tid_after, tpidr_after, sp_after
        ));
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
            let elr: u64;
            unsafe {
                core::arch::asm!("mrs {}, far_el1", out(reg) far);
                core::arch::asm!("mrs {}, elr_el1", out(reg) elr);
            }
            crate::console::print(&alloc::format!(
                "[Fault] Data abort from EL0 at FAR={:#x}, ELR={:#x}, ISS={:#x}\n",
                far,
                elr,
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
