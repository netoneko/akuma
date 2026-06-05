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

    /* Boot-stack reservation, derived from the *actual* linked image size so it
     * auto-tracks the binary — there is no hand-tuned per-profile IMAGE_SIZE
     * constant anymore. The boot stack is placed immediately above the kernel
     * image (page-aligned, with a 2-page guard gap) and is 1 MB:
     *   STACK_BOTTOM  = first page above the image + 2-page guard
     *   STACK_TOP     = initial SP (boot.rs asm loads it); 1 MB above STACK_BOTTOM
     *   IMAGE_RESERVE = bytes from the load address to STACK_BOTTOM, for the
     *                   ARM64 Image header (QEMU DTB placement)
     * boot.rs (asm SP + Image header), src/main.rs (overlap guard + heap reserve +
     * ExecConfig boot-stack bounds) and src/exceptions.rs (boot-thread exception
     * stack) all read these as extern absolute symbols — one source of truth. */
    STACK_BOTTOM  = ALIGN(_kernel_phys_end, 0x1000) + 0x2000;
    STACK_TOP     = STACK_BOTTOM + 0x100000;
    IMAGE_RESERVE = STACK_BOTTOM - KERNEL_PHYS_BASE;

    /DISCARD/ : {
        *(.eh_frame)
    }
}

/*
 * Boot stack layout.
 * STACK_BOTTOM / STACK_TOP / IMAGE_RESERVE are derived from _kernel_phys_end in
 * the SECTIONS block above, so the boot-stack reservation always sits above the
 * kernel image by construction and auto-tracks the binary on every build. The
 * old ASSERT(_kernel_phys_end < STACK_BOTTOM) and the per-profile IMAGE_SIZE /
 * --defsym=STACK_BOTTOM machinery (build.rs + boot.rs constants) are gone — there
 * is nothing left to keep in lockstep. The check below can no longer fail; it is
 * kept only as a self-documenting invariant.
 */
ASSERT(_kernel_phys_end < STACK_BOTTOM,
    "FATAL: boot-stack reservation underflowed the kernel image (linker.ld bug)")
