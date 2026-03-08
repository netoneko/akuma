ENTRY(_boot)

/* Kernel virtual address base - upper half of address space */
KERNEL_VIRT_BASE = 0xFFFF000080000000;

/* Physical load address - Firecracker DRAM starts at 2 GB */
KERNEL_PHYS_BASE = 0x80000000;

/* Offset to convert between physical and virtual */
KERNEL_OFFSET = KERNEL_VIRT_BASE - KERNEL_PHYS_BASE;

SECTIONS
{
    . = KERNEL_PHYS_BASE;

    /* Boot code - runs before MMU is enabled */
    .text.boot : {
        KEEP(*(.text._boot))
        KEEP(*(.text.boot))
    }

    /* Boot data - also at physical addresses for pre-MMU access */
    .data.boot : {
        *(.data.boot)
    }

    /* Main kernel code */
    _text_start = .;
    .text : {
        *(.text .text.*)
    }

    /* Exception vector table - must be 2KB aligned */
    .text.exceptions : ALIGN(0x800) {
        *(.text.exceptions)
    }
    _text_end = .;

    /* Read-only data - page align for separate protection */
    . = ALIGN(0x1000);
    _rodata_start = .;
    .rodata : {
        *(.rodata .rodata.*)
    }
    _rodata_end = .;

    /* Writable data - page align for separate protection */
    . = ALIGN(0x1000);
    _data_start = .;
    .data : {
        *(.data .data.*)
    }

    .bss (NOLOAD) : {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    }
    _data_end = .;

    /* Record where kernel ends (physical) */
    _kernel_phys_end = .;

    /DISCARD/ : {
        *(.eh_frame)
    }
}

/*
 * Boot stack layout check (Firecracker):
 *   KERNEL_PHYS_BASE = 0x80000000
 *   STACK_TOP        = 0x80800000 (8MB from base)
 *   STACK_SIZE       = 0x100000   (1MB)
 *   STACK_BOTTOM     = 0x80700000 (7MB from base)
 *
 * Kernel binary must fit below STACK_BOTTOM.
 */
STACK_BOTTOM = 0x80700000;
ASSERT(_kernel_phys_end < STACK_BOTTOM,
    "FATAL: Kernel binary overlaps boot stack! Kernel end must be < 0x80700000 (7MB from Firecracker base).")
