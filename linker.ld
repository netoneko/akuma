ENTRY(_boot)

SECTIONS
{
    /* QEMU virt machine loads kernel at 0x40000000 */
    . = 0x40000000;
    
    .text : {
        KEEP(*(.text._boot))
        *(.text .text.*)
    }
    
    .rodata : {
        *(.rodata .rodata.*)
    }
    
    .data : {
        *(.data .data.*)
    }
    
    .bss : {
        *(.bss .bss.*)
    }
    
    /DISCARD/ : {
        *(.eh_frame)
    }
}

