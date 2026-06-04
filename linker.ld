ENTRY(_boot)

/* Kernel virtual address base - upper half of address space */
KERNEL_VIRT_BASE = 0xFFFF000040000000;

/* Physical load address - where QEMU loads the kernel.
 * When using flat binary with ARM64 Image header, QEMU loads at
 * RAM_BASE + text_offset. With text_offset < 4KB, QEMU adds 2MB,
 * so we get RAM_BASE + 2MB = 0x40200000.
 *
 * Note: The first 2MB of RAM (0x40000000-0x401FFFFF) can be used
 * for the DTB which QEMU places there for flat binary boots.
 */
KERNEL_PHYS_BASE = 0x40200000;

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
 * Boot stack layout check.
 * STACK_BOTTOM = KERNEL_PHYS_LOAD + IMAGE_SIZE, set per profile by build.rs via
 * --defsym=STACK_BOTTOM=VALUE. We deliberately do NOT PROVIDE a fallback here:
 * LLD lets a linker-script PROVIDE override an external --defsym, so a fallback
 * would silently mask the real per-profile value and the ASSERT below would
 * compare against the wrong (always-passing) bound. cargo always passes the
 * --defsym (see build.rs), so STACK_BOTTOM is always defined for real builds; a
 * manual `ld` without it fails loudly on the undefined symbol, which is correct.
 *   size profile:        0x40200000 + 0x0EC000 = 0x402EC000
 *   extreme-size profile:0x40200000 + 0x0DC000 = 0x402DC000
 *   release profile:     0x40200000 + 0x300000 = 0x40500000
 *
 * Kernel binary must fit below STACK_BOTTOM.
 * This assertion fails the build if kernel is too large.
 */
ASSERT(_kernel_phys_end < STACK_BOTTOM,
    "FATAL: Kernel binary overlaps boot stack! Kernel binary too large for declared IMAGE_SIZE. Reduce kernel size or increase IMAGE_SIZE in boot.rs/build.rs.")
