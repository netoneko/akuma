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
    .text : {
        *(.text .text.*)
    }
    
    /* Exception vector table - must be 2KB aligned */
    .text.exceptions : ALIGN(0x800) {
        *(.text.exceptions)
    }
    
    .rodata : {
        *(.rodata .rodata.*)
    }
    
    .data : {
        *(.data .data.*)
    }
    
    .bss : {
        __bss_start = .;
        *(.bss .bss.*)
        __bss_end = .;
    }
    
    /* Boot BSS - at physical address but after main BSS */
    .bss.boot : {
        *(.bss.boot)
    }
    
    /* Record where kernel ends (physical) */
    _kernel_phys_end = .;
    
    /DISCARD/ : {
        *(.eh_frame)
    }
}
