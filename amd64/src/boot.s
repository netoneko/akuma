/* amd64 boot: PVH entry (32-bit protected mode) -> long mode -> kmain.
 *
 * AT&T syntax deliberately. The one instruction this file cannot avoid is the
 * far jump that activates 64-bit mode, and `ljmp $0x08, $target` is
 * unambiguous in AT&T where the Intel-syntax spelling varies between
 * assemblers. Everything else follows for consistency.
 *
 * # Why PVH, and not multiboot or the 64-bit Linux boot protocol
 *
 * The target is Firecracker on x86_64. Firecracker picks the boot protocol
 * from the kernel ELF itself: `configure_system_for_boot` matches on
 * `entry_point.protocol`, and an ELF that declares the PVH note gets
 * `BootProtocol::PvhBoot` rather than `BootProtocol::LinuxBoot`. Declaring the
 * note is therefore the whole mechanism — there is no Firecracker-side switch.
 *
 * PVH is the right half of that choice for one reason: QEMU implements it too.
 * `qemu-system-x86_64 -kernel <elf>` finds the same note and enters the same
 * way, so a local run and a Firecracker run exercise one entry path instead of
 * two. The 64-bit LinuxBoot path would have handed us a vCPU already in long
 * mode with paging on — less code here — at the cost of having no way to
 * reproduce that state locally, which is the more expensive trade while this
 * file is still being written.
 *
 * # Entry state (PVH / x86 HVM direct boot ABI)
 *
 * 32-bit protected mode, paging OFF, interrupts OFF, CS a flat 32-bit code
 * segment and the data segments flat. %ebx holds the physical address of the
 * `hvm_start_info` block. No stack is provided, so %esp is the first thing to
 * fix. Everything the trampoline below does — page tables, PAE, EFER.LME, the
 * far jump — is work this ABI leaves to the kernel.
 */

/* ---- PVH ELF note ------------------------------------------------------- */
/* The loader (rust-vmm `linux-loader` for Firecracker, `pvh_load_kernel` for
 * QEMU) scans PT_NOTE program headers for name "Xen\0" and type 18
 * (XEN_ELFNOTE_PHYS32_ENTRY), then reads a little-endian u32 entry address out
 * of the descriptor. Both namesz and descsz are padded to 4 bytes; ours are
 * exactly 4 already.
 *
 * This must land in a section the linker gives SHT_NOTE and covers with a
 * PT_NOTE phdr — linker.ld does that explicitly rather than trusting lld to
 * synthesise it, because a note present in the file but not in any program
 * header is invisible to the loader, and the symptom is a silent fallback to
 * the 64-bit protocol. */
.set XEN_ELFNOTE_PHYS32_ENTRY, 18

.section .note.Xen, "a", @note
.align 4
    .long 4                              /* namesz: "Xen\0" */
    .long 4                              /* descsz: one u32 */
    .long XEN_ELFNOTE_PHYS32_ENTRY       /* type 18 */
    .asciz "Xen"                         /* 4 bytes including the NUL */
    .long _start                         /* 32-bit entry point */

/* ---- 32-bit entry ------------------------------------------------------- */
.section .text.boot, "ax"
.code32
.globl _start
.type _start, @function
_start:
    cli
    cld
    movl $__boot_stack_top, %esp

    /* Stash the hvm_start_info pointer somewhere `rep stosl` will not eat.
     * It has to survive until the call to kmain, which wants it in %rdi. */
    movl %ebx, %esi

    /* Zero all three page tables in one pass. .bss is NOLOAD, and while both
     * loaders do zero memsz-filesz, relying on that would make a garbage PML4
     * entry a silent triple-fault instead of a visible bug. */
    movl $__pml4, %edi
    xorl %eax, %eax
    movl $(3 * 4096 / 4), %ecx
    rep stosl

    /* PML4[0] -> PDPT, present + writable */
    movl $__pdpt, %eax
    orl  $0x03, %eax
    movl %eax, __pml4

    /* PDPT[0] -> PD, present + writable */
    movl $__pd, %eax
    orl  $0x03, %eax
    movl %eax, __pdpt

    /* PD[i] = (i * 2 MiB) | present | writable | PS  -> identity-map 1 GiB.
     * 2 MiB pages rather than a single 1 GiB PDPT entry on purpose: 1 GiB
     * pages need CPUID PDPE1GB, which the default `qemu64` CPU does not
     * advertise, and the failure mode would be a triple-fault at `mov %cr0`
     * with nothing on the serial line. */
    xorl %ecx, %ecx
    movl $0x83, %eax
1:
    movl %eax, __pd(, %ecx, 8)
    addl $0x200000, %eax
    incl %ecx
    cmpl $512, %ecx
    jb 1b


    /* CR4.PAE — required before EFER.LME has any effect. */
    movl %cr4, %eax
    orl  $(1 << 5), %eax
    movl %eax, %cr4

    movl $__pml4, %eax
    movl %eax, %cr3

    /* EFER.LME (MSR 0xC0000080 bit 8): long mode enabled, not yet active. */
    movl $0xC0000080, %ecx
    rdmsr
    orl  $(1 << 8), %eax
    wrmsr

    /* CR0.PG | CR0.PE — paging on. LME + PG is what makes long mode active;
     * the CPU is in compatibility mode from here until the far jump loads a
     * descriptor with L=1. */
    movl %cr0, %eax
    orl  $((1 << 31) | (1 << 0)), %eax
    movl %eax, %cr0

    lgdt __gdt64_ptr
    ljmp $0x08, $long_mode_start

/* ---- 64-bit entry ------------------------------------------------------- */
.code64
long_mode_start:
    /* Every data segment register to the flat data descriptor. In long mode
     * most of these are ignored, but leaving them holding stale 32-bit
     * selectors makes any later descriptor-table change behave oddly. */
    movw $0x10, %ax
    movw %ax, %ss
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %fs
    movw %ax, %gs

    movq $__boot_stack_top, %rsp
    xorq %rbp, %rbp

    /* System V: first argument in %rdi. %esi still holds the hvm_start_info
     * pointer; the 32-bit move zero-extends, which is what we want — the PVH
     * ABI defines it as a 32-bit physical address. */
    movl %esi, %edi
    call kmain

    /* kmain is `-> !`; this is the belt-and-braces halt if that ever changes. */
2:
    cli
    hlt
    jmp 2b

/* ---- descriptors -------------------------------------------------------- */
.section .rodata
.align 16
__gdt64:
    .quad 0                          /* null descriptor */
    .quad 0x00AF9A000000FFFF         /* 0x08: code, ring 0, L=1 (64-bit) */
    .quad 0x00CF92000000FFFF         /* 0x10: data, ring 0, writable */
__gdt64_end:

__gdt64_ptr:
    .word __gdt64_end - __gdt64 - 1
    .quad __gdt64                    /* `lgdt` in 32-bit mode reads only the
                                      * low 4 bytes of this; the upper half is
                                      * harmless padding that also makes the
                                      * same descriptor valid once in 64-bit
                                      * mode. */

/* ---- boot page tables and stack ----------------------------------------- */
.section .bss.pagetables, "aw", @nobits
.align 4096
__pml4: .skip 4096
__pdpt: .skip 4096
__pd:   .skip 4096

.section .bss, "aw", @nobits
.align 16
__boot_stack:
    .skip 64 * 1024
__boot_stack_top:
