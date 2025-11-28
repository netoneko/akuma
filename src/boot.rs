use core::arch::global_asm;

global_asm!(
    ".section .text._boot",
    ".global _boot",
    "_boot:",
    "    mov x19, x0",         // Save DTB pointer from x0 to x19
    "    ldr x0, =0x40100000", // Load stack address
    "    mov sp, x0",          // Set stack pointer
    "    mov x0, x19",         // Restore DTB pointer as first argument
    "    bl _start",           // Call Rust _start with DTB pointer
    "hang:",
    "    wfe",
    "    b hang"
);
