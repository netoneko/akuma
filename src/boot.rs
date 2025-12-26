use core::arch::global_asm;

global_asm!(
    ".section .text._boot",
    ".global _boot",
    "_boot:",
    "    mov x19, x0", // Save DTB pointer from x0 to x19
    // Enable FPU/SIMD (CPACR_EL1: set FPEN bits 20-21 to 0b11)
    "    mov x0, #(3 << 20)",  // FPEN = 0b11: no trapping of FP/SIMD
    "    msr cpacr_el1, x0",   // Write to CPACR_EL1
    "    isb",                 // Instruction barrier
    "    ldr x0, =0x40100000", // Load stack address
    "    mov sp, x0",          // Set stack pointer
    "    mov x0, x19",         // Restore DTB pointer as first argument
    "    bl rust_start",       // Call Rust main with DTB pointer
    "hang:",
    "    wfe",
    "    b hang"
);
