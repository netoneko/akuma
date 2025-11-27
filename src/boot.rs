use core::arch::global_asm;

global_asm!(
    ".section .text._boot",
    ".global _boot",
    "_boot:",
    "    ldr x0, =0x40100000",
    "    mov sp, x0",
    "    bl _start",
    "hang:",
    "    wfe",
    "    b hang"
);
