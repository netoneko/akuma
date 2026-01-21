ENTRY(_boot)

/* Kernel virtual address base - upper half of address space */
KERNEL_VIRT_BASE = 0xFFFF000040000000;

/* Physical load address - where QEMU loads the kernel */
KERNEL_PHYS_BASE = 0x40000000;

/* Offset to convert between physical and virtual */
KERNEL_OFFSET = KERNEL_VIRT_BASE - KERNEL_PHYS_BASE;

SECTIONS
{
    /* 
     * All code starts at physical address.
     * Boot code runs with MMU off, rest runs with MMU on.
     * We use one contiguous layout at physical addresses,
     * and the kernel accesses everything via virtual addresses
     * once MMU is enabled.
     */
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
        /* Capture all BSS sections EXCEPT .bss.boot */
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    }
    _data_end = .;
    
    /* Boot BSS - contains boot page tables */
    /* Note: .bss.boot will be merged into .bss above due to .bss.* pattern */
    /* The boot_page_tables symbol location doesn't matter as long as it's */
    /* within the kernel's BSS and before _kernel_phys_end */
    
    /* Record where kernel ends (physical) */
    _kernel_phys_end = .;
    
    /DISCARD/ : {
        *(.eh_frame)
    }
}

/*
 * Boot stack layout check:
 *   KERNEL_PHYS_BASE = 0x40000000
 *   STACK_TOP        = 0x42000000 (32MB from base)
 *   STACK_SIZE       = 0x100000   (1MB)
 *   STACK_BOTTOM     = 0x41F00000 (31MB from base)
 *
 * Kernel binary must fit below STACK_BOTTOM.
 * This assertion fails the build if kernel is too large.
 */
STACK_BOTTOM = 0x41F00000;
ASSERT(_kernel_phys_end < STACK_BOTTOM, 
    "FATAL: Kernel binary overlaps boot stack! Kernel end must be < 0x41F00000 (31MB from base). Reduce kernel size or increase STACK_TOP in boot.rs.")
