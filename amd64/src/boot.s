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

/* ---- multiboot2 header -------------------------------------------------- */
/* The second way in. GRUB cannot boot a PVH image — that note means nothing to
 * it — so bare metal needs a protocol GRUB does speak, and multiboot2 is the
 * one that costs least: the machine state it hands over is **almost exactly**
 * the PVH state (32-bit protected mode, paging off, interrupts off, flat
 * segments, a pointer to an info block in %ebx), so the two entries differ in
 * three instructions and then share every line of the trampoline below.
 *
 * The header must be 8-byte aligned and within the first 32 KiB of the *file*;
 * linker.ld places it immediately after the Xen note, a few hundred bytes in.
 *
 * Tags, in order: entry address (so GRUB comes to `_start_mb2` and not to
 * `e_entry`, which is the PVH entry and would leave %eax's magic unchecked and
 * %ebx pointing at the wrong kind of block); framebuffer (this machine has no
 * UART, so a framebuffer is not decoration, it is the only console there is);
 * module alignment (so a GRUB module lands on a page boundary); end. */
.set MB2_MAGIC,        0xE85250D6
.set MB2_ARCH_I386,    0

.section .multiboot2, "a"
.align 8
mb2_header:
    .long MB2_MAGIC
    .long MB2_ARCH_I386
    .long mb2_header_end - mb2_header
    .long -(MB2_MAGIC + MB2_ARCH_I386 + (mb2_header_end - mb2_header))

    /* Entry address tag: come to the multiboot2 entry, not to e_entry. */
    .align 8
    .short 3
    .short 0
    .long 12
    .long _start_mb2

    /* Framebuffer tag. Zero width and height mean "no preference": GRUB gives
     * whatever mode it has already set, which the menu entry chooses with
     * `set gfxmode`. Asking for a specific size here and not getting it is a
     * refusal to boot; asking for none and reading back what we got is a
     * console that works at whatever the firmware could manage. Depth 32 is
     * the one real request — every x86 GOP offers it. */
    .align 8
    .short 5
    .short 0
    .long 20
    .long 0
    .long 0
    .long 32

    /* Module alignment: page-align anything loaded alongside us. */
    .align 8
    .short 6
    .short 0
    .long 8

    /* End tag. */
    .align 8
    .short 0
    .short 0
    .long 8
mb2_header_end:

/* ---- 32-bit entry ------------------------------------------------------- */
/* Which loader we came in through. Carried in %edx across the whole
 * trampoline — registers survive the far jump, so this needs no memory and
 * cannot be clobbered by the page-table zeroing below. */
.set BOOT_PVH,        1
.set BOOT_MULTIBOOT2, 2

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
    movl $BOOT_PVH, %edx
    jmp common32

/* GRUB's entry. %eax holds 0x36D76289 and %ebx the multiboot2 info block.
 *
 * The magic is deliberately NOT checked here. There is nowhere to report a
 * failure to — no serial port on the target machine, and the framebuffer's
 * address is inside the very block the magic would be vouching for — so a
 * check could only halt silently, which is what a wrong-magic boot does
 * anyway. `kmain_mb2` validates the block itself, where a failure can at
 * least paint the screen. */
.globl _start_mb2
.type _start_mb2, @function
_start_mb2:
    cli
    cld
    movl $__boot_stack_top, %esp
    movl %ebx, %esi
    movl $BOOT_MULTIBOOT2, %edx
    jmp common32

common32:
    /* Zero every page table in one pass. .bss is NOLOAD, and while both
     * loaders do zero memsz-filesz, relying on that would make a garbage PML4
     * entry a silent triple-fault instead of a visible bug. */
    movl $__pml4, %edi
    xorl %eax, %eax
    movl $(8 * 4096 / 4), %ecx
    rep stosl

    /* PML4[0] -> the low PDPT: the identity map the trampoline runs on, and
     * which stays live until the kernel has switched to high addresses. */
    movl $__pdpt_low, %eax
    orl  $0x03, %eax
    movl %eax, __pml4

    /* PML4[256] -> the high PDPT: the physmap window at 0xFFFF_8000_0000_0000,
     * where the kernel image is linked and through which the kernel reaches any
     * physical page. Slot 256 is the first of the upper half; entry size is 8,
     * so the byte offset is 256*8 = 2048. */
    movl $__pdpt_high, %eax
    orl  $0x03, %eax
    movl %eax, __pml4 + 2048

    /* PML4[511] -> the kernel PDPT: the top -2 GiB, where the image is linked.
     * Slot 511 is at byte offset 511*8 = 4088. */
    movl $__pdpt_kern, %eax
    orl  $0x03, %eax
    movl %eax, __pml4 + 4088

    /* The low PDPT and the physmap PDPT point at the SAME four page directories.
     * The identity map and the physmap describe identical memory, so sharing
     * costs four frames less and makes it impossible for the two views to
     * disagree.
     *
     * **Four** page directories, covering the low 4 GiB, where there used to be
     * one covering 1 GiB. That is not generosity: on real hardware the things a
     * kernel must reach before it can allocate anything are all up there. The
     * framebuffer this machine's only console lives in is a PCI BAR at
     * 0xE0000000 — 3.5 GiB — and with a 1 GiB map the first write to it faults
     * before there is any way to say so. A VMM guest never noticed because a
     * VMM puts everything low. */
    movl $__pd0, %eax
    orl  $0x03, %eax
    movl %eax, __pdpt_low + 0
    movl %eax, __pdpt_high + 0
    /* 0xFFFFFFFF80000000 falls in PDPT slot 510 (byte offset 510*8 = 4080), so
     * that is where the kernel image's first GiB is described. */
    movl %eax, __pdpt_kern + 4080
    addl $4096, %eax
    movl %eax, __pdpt_low + 8
    movl %eax, __pdpt_high + 8
    addl $4096, %eax
    movl %eax, __pdpt_low + 16
    movl %eax, __pdpt_high + 16
    addl $4096, %eax
    movl %eax, __pdpt_low + 24
    movl %eax, __pdpt_high + 24

    /* PD[i] = (i * 2 MiB) | present | writable | PS  -> identity-map 1 GiB.
     * 2 MiB pages rather than a single 1 GiB PDPT entry on purpose: 1 GiB
     * pages need CPUID PDPE1GB, which the default `qemu64` CPU does not
     * advertise, and the failure mode would be a triple-fault at `mov %cr0`
     * with nothing on the serial line. */
    /* 2048 entries across the four contiguous directories = 4 GiB. The four are
     * declared adjacently in .bss.pagetables precisely so one loop can index
     * them as a single array; splitting them apart breaks this silently. */
    xorl %ecx, %ecx
    movl $0x83, %eax
1:
    movl %eax, __pd0(, %ecx, 8)
    addl $0x200000, %eax
    incl %ecx
    cmpl $2048, %ecx
    jb 1b


    /* CR4.PAE — required before EFER.LME has any effect. */
    movl %cr4, %eax
    orl  $(1 << 5), %eax
    movl %eax, %cr4

    movl $__pml4, %eax
    movl %eax, %cr3

    /* EFER (MSR 0xC0000080): LME bit 8, NXE bit 11.
     *
     * LME enables long mode — not yet active; LME + CR0.PG is what activates it.
     *
     * **NXE is not optional and is easy to miss.** Without it, bit 63 of a page
     * table entry is a *reserved* bit rather than the no-execute flag: setting
     * it does not mark a page non-executable, it makes every access to that page
     * fault with the reserved-bit error. So a kernel that omits NXE and then
     * tries to enforce W^X gets the opposite of what it asked for, and gets it as
     * a fault rather than a silent downgrade. Set here, in the same `wrmsr` as
     * LME, so no page-table code can run before it is in force. */
    movl $0xC0000080, %ecx
    rdmsr
    orl  $((1 << 8) | (1 << 11)), %eax
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

    /* Make SSE legal for ring 3.
     *
     * The kernel is built for `x86_64-unknown-none`, which is soft-float, so
     * nothing in it emits an xmm instruction. A ring-3 binary the tree did not
     * compile (busybox, any normal musl program) emits `movups`/xmm in its
     * startup and #UDs unless CR4.OSFXSR is set: SSE is architecturally present
     * on every x86_64 part but the OS has to acknowledge it.
     *
     *   CR0.EM (bit 2) = 0  — do NOT trap x87/SSE as "no coprocessor"
     *   CR0.MP (bit 1) = 1  — `wait`/`fwait` honour TS
     *   CR4.OSFXSR      (bit  9) = 1  — enable SSE + `fxsave`/`fxrstor` layout
     *   CR4.OSXMMEXCPT  (bit 10) = 1  — deliver #XM instead of #UD on an SSE fault
     *
     * There is no `fxsave`/`fxrstor` on the context switch yet (README: "No
     * FP/SIMD state save"), so this only makes the instructions legal — a
     * second SSE-using task would clobber the first's xmm registers. */
    movq %cr0, %rax
    andq $-5, %rax
    orq  $2, %rax
    movq %rax, %cr0
    movq %cr4, %rax
    orq  $((1 << 9) | (1 << 10)), %rax
    movq %rax, %cr4

    /* Still executing from the low identity map. Jump to the kernel's linked
     * (high) address: an absolute indirect jump, because a direct `jmp` encodes
     * a 32-bit displacement and the target is 2^47 away. */
    movabsq $high_entry, %rax
    jmpq *%rax

    .section .text
high_entry:
    /* Now running from the top -2 GiB. Rebase the stack into the physmap: it
     * aliases the identity map, so this points at the very bytes the trampoline
     * was already using — no copy, no discontinuity. The stack cannot stay at
     * its identity address, because that mapping is what gets dropped to hand
     * the lower half to userspace. */
    movabsq $0xFFFF800000000000, %rax
    addq %rax, %rsp

    /* System V: first argument in %rdi. %esi still holds the info-block
     * pointer; the 32-bit move zero-extends, which is what we want — both ABIs
     * define it as a 32-bit physical address.
     *
     * %edx has survived from the entry stub and says which loader put us here.
     * The two kernels-in-waiting take different blocks: `kmain` reads an
     * `hvm_start_info`, `kmain_mb2` reads a multiboot2 info structure. Handing
     * either the other's block is a boot that dereferences plausible garbage. */
    movl %esi, %edi
    cmpl $BOOT_MULTIBOOT2, %edx
    je 3f
    call kmain
    jmp 2f
3:
    call kmain_mb2

    /* kmain is `-> !`; this is the belt-and-braces halt if that ever changes. */
2:
    cli
    hlt
    jmp 2b

/* ---- descriptors -------------------------------------------------------- */
/* In the low boot region, not the kernel's .rodata: `lgdt` runs in the 32-bit
 * trampoline with paging off, so the descriptor table must be reachable at its
 * physical address. Left in .rodata it linked to a high VMA and the relocation
 * would not fit — "R_X86_64_32 out of range ... references section '.rodata'". */
.section .rodata.boot, "a"
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

/* ---- AP trampoline ------------------------------------------------------ */
/* What a secondary core executes from the moment it leaves reset. A STARTUP
 * IPI names a 4 KiB page below 1 MiB and the core begins there in 16-bit real
 * mode with CS:IP = (page << 8):0000, so this block lives in the kernel's
 * .rodata (a high address, like everything else) and `smp::start_secondaries`
 * COPIES it to physical AP_BASE before the IPI. Every address in it is
 * therefore spelled as `AP_BASE + (label - ap_trampoline_start)` — a constant
 * the assembler folds — and never as a bare label, which would name the
 * .rodata copy that the core cannot see.
 *
 * It is the 32-bit trampoline above, replayed for a core that arrives in an
 * older mode still, and with two differences: the page tables already exist
 * (the BSP built them and left the root in the mailbox), and the far jumps go
 * through a GDT of their own whose 0x08 and 0x10 are byte-identical to the
 * kernel's, so the CS/SS the core arrives in long mode with stay valid when
 * `gdt::init_cpu` loads the real table. 0x18 is the one extra entry: a 32-bit
 * code segment for the real->protected jump, which cannot go through an L=1
 * descriptor.
 *
 * The mailbox is the top 256 bytes of the page. The BSP fills it, sends the
 * IPI, waits for the core to report in, then reuses it for the next one. */
.set AP_BASE,     0x8000
.set AP_MB_CR3,   AP_BASE + 0xF00
.set AP_MB_STACK, AP_BASE + 0xF08
.set AP_MB_ENTRY, AP_BASE + 0xF10
.set AP_MB_INDEX, AP_BASE + 0xF18

.section .rodata
.align 16
.globl ap_trampoline_start
ap_trampoline_start:
.code16
    cli
    cld
    xorw %ax, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    /* `lgdtl`: the 32-bit form, so the descriptor's base is read as a full
     * 32-bit linear address rather than 24 bits. */
    lgdtl AP_BASE + (ap_gdt_ptr - ap_trampoline_start)
    movl %cr0, %eax
    orl  $1, %eax
    movl %eax, %cr0
    ljmpl $0x18, $(AP_BASE + (ap32 - ap_trampoline_start))

.code32
ap32:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw %ax, %fs
    movw %ax, %gs

    /* CR4.PAE, the root from the mailbox, EFER.LME|NXE, CR0.PG — the same
     * sequence as `common32`, for the same reasons; see there. */
    movl %cr4, %eax
    orl  $(1 << 5), %eax
    movl %eax, %cr4

    movl AP_MB_CR3, %eax
    movl %eax, %cr3

    movl $0xC0000080, %ecx
    rdmsr
    orl  $((1 << 8) | (1 << 11)), %eax
    wrmsr

    movl %cr0, %eax
    orl  $((1 << 31) | (1 << 0)), %eax
    movl %eax, %cr0

    ljmpl $0x08, $(AP_BASE + (ap64 - ap_trampoline_start))

.code64
ap64:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw %ax, %fs
    movw %ax, %gs

    /* The stack the BSP allocated (a high address, mapped because the root is
     * the kernel's own PML4 plus an identity slot), this core's index as the
     * first argument, and the high entry point. `jmp`, not `call`: the entry
     * never returns, and the BSP already lowered the stack by one word so the
     * callee sees the alignment `call` would have given it. */
    movq AP_MB_STACK, %rsp
    movq AP_MB_INDEX, %rdi
    movq AP_MB_ENTRY, %rax
    jmpq *%rax

.align 16
ap_gdt:
    .quad 0
    .quad 0x00AF9A000000FFFF         /* 0x08: code, ring 0, L=1 — as the kernel's */
    .quad 0x00CF92000000FFFF         /* 0x10: data, ring 0        — as the kernel's */
    .quad 0x00CF9A000000FFFF         /* 0x18: code, ring 0, D=1 (32-bit), boot only */
ap_gdt_end:
ap_gdt_ptr:
    .word ap_gdt_end - ap_gdt - 1
    .long AP_BASE + (ap_gdt - ap_trampoline_start)
.globl ap_trampoline_end
ap_trampoline_end:

/* ---- boot page tables and stack ----------------------------------------- */
.section .bss.pagetables, "aw", @nobits
.align 4096
__pml4:      .skip 4096
__pdpt_low:  .skip 4096
__pdpt_high: .skip 4096
__pdpt_kern: .skip 4096
/* The four page directories are last and adjacent so the fill loop above can
 * treat them as one 2048-entry array. Keep them together. */
__pd0:       .skip 4096
__pd1:       .skip 4096
__pd2:       .skip 4096
__pd3:       .skip 4096

/* The boot stack is addressed physically by the 32-bit trampoline, so it lives
 * in .bootbss beside the page tables rather than in the high .bss. */
.section .bss.bootstack, "aw", @nobits
.align 16
__boot_stack:
    .skip 64 * 1024
__boot_stack_top:
