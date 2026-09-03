/* amd64 kernel layout: low trampoline, high kernel.
 *
 * The kernel is linked to run at `KERNEL_VMA + physical`, in the upper half,
 * and *loaded* at its physical address. Two regions therefore have different
 * rules:
 *
 *   .boot / .bootbss  — VMA == LMA, low.  The PVH entry runs here in 32-bit
 *                       protected mode with paging off, so every address it
 *                       touches must be physical. That includes the boot page
 *                       tables and the boot stack, which is why they are here
 *                       and not in the ordinary .bss.
 *   everything else   — VMA high, LMA low, via AT().
 *
 * `KERNEL_VMA` is the top -2 GiB, which is what `-C code-model=kernel` assumes.
 * The physmap — through which the kernel reaches an arbitrary physical page —
 * is a second window at PML4 slot 256; the two are separate because a
 * kernel-code-model image cannot be linked into slot 256's range.
 *
 * Load address 0x200000 (2 MiB) still clears the region both loaders use for
 * boot structures — Firecracker puts hvm_start_info at 0x6000 and the command
 * line at 0x20000.
 */
ENTRY(_start)

/* Top -2 GiB: the range `-C code-model=kernel` is built for. The physmap is a
 * *separate* window (PML4 slot 256, 0xFFFF800000000000) since a kernel-model
 * image cannot be linked there. */
KERNEL_VMA = 0xFFFFFFFF80000000;

PHDRS
{
    note    PT_NOTE FLAGS(4);           /* R   — the PVH note */
    boot    PT_LOAD  FLAGS(5);          /* R+X — the low trampoline */
    text    PT_LOAD  FLAGS(5);          /* R+X */
    rodata  PT_LOAD  FLAGS(4);          /* R   */
    data    PT_LOAD  FLAGS(6);          /* R+W — .data and .bss */
}

SECTIONS
{
    . = 0x200000;

    _kernel_start = .;

    /* KEEP: nothing here is referenced by any Rust symbol, so --gc-sections
     * would otherwise be free to drop the one structure that decides how the
     * kernel is booted at all. */
    .note.Xen : { KEEP(*(.note.Xen)) } :note

    . = ALIGN(0x1000);
    .boot : {
        KEEP(*(.text.boot))
        /* The GDT the trampoline loads: physical, for the same reason the code
         * around it is. */
        KEEP(*(.rodata.boot))
    } :boot

    /* Boot-time BSS, addressed physically by the trampoline.
     * Page tables first and contiguous: boot.s zeroes all three with one
     * `rep stosl` from __pml4, which is only correct while they abut. */
    . = ALIGN(0x1000);
    .bootbss (NOLOAD) : {
        *(.bss.pagetables)
        *(.bss.bootstack)
    } :boot

    . = ALIGN(0x1000);
    _kernel_phys_body = .;

    /* From here on: high VMA, low LMA. */
    . += KERNEL_VMA;

    .text ALIGN(0x1000) : AT(ADDR(.text) - KERNEL_VMA) {
        *(.text .text.*)
    } :text

    .rodata ALIGN(0x1000) : AT(ADDR(.rodata) - KERNEL_VMA) {
        *(.rodata .rodata.*)
    } :rodata

    .data ALIGN(0x1000) : AT(ADDR(.data) - KERNEL_VMA) {
        *(.data .data.*)
    } :data

    .bss ALIGN(0x1000) (NOLOAD) : AT(ADDR(.bss) - KERNEL_VMA) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    } :data

    /* Physical end of the image, for the PMM reservation. `.` is a high VMA
     * here, so the offset has to come back off. */
    _kernel_end = . - KERNEL_VMA;

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
        *(.note.GNU-stack)
        *(.note.gnu.property)
    }
}

ASSERT(_kernel_start == 0x200000, "amd64 image must load at 2 MiB")
