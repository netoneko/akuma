/* Linker script for Akuma userspace programs */
/* Programs are loaded at 0x400000 (4MB mark) */

ENTRY(_start)

SECTIONS
{
    /* User programs start at 4MB */
    . = 0x400000;
    
    .text : {
        *(.text._start)
        *(.text .text.*)
    }
    
    .rodata : {
        *(.rodata .rodata.*)
    }
    
    /* Align to page boundary before writable sections */
    . = ALIGN(0x1000);
    
    .data : {
        *(.data .data.*)
    }
    
    .bss : {
        __bss_start = .;
        *(.bss .bss.*)
        __bss_end = .;
    }
    
    /DISCARD/ : {
        *(.eh_frame)
        *(.comment)
        *(.ARM.attributes)
    }
}

