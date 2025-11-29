use core::arch::global_asm;

// Global variable to store DTB pointer from assembly
#[unsafe(no_mangle)]
static mut DTB_FROM_BOOT: usize = 0xDEADBEEF;

global_asm!(
    ".section .text._boot",
    ".global _boot",
    "_boot:",
    "    mov x19, x0",              // Save DTB pointer from x0 to x19
    "    ldr x1, =DTB_FROM_BOOT",   // Load address of global variable
    "    str x19, [x1]",            // Store x0 value to prove boot ran
    "    ldr x0, =0x40100000",      // Load stack address
    "    mov sp, x0",               // Set stack pointer
    "    mov x0, x19",              // Restore DTB pointer as first argument
    "    bl rust_start",            // Call Rust main with DTB pointer
    "hang:",
    "    wfe",
    "    b hang"
);
