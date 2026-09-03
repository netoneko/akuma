/* amd64 kernel layout.
 *
 * Load address 0x200000 (2 MiB). Two constraints pin it:
 *   - it must clear the region both loaders use for boot structures. Firecracker
 *     places hvm_start_info at 0x6000 and the command line at 0x20000, and the
 *     legacy sub-1 MiB area holds the BIOS data area and device memory.
 *   - boot.s identity-maps the first 1 GiB with 2 MiB pages, so the image has
 *     to land inside that window.
 *
 * PHDRS is explicit rather than left to lld. The PVH note is only found by a
 * loader that can see it through a PT_NOTE program header; a note that exists
 * as a section but is covered by no phdr is invisible, and the failure is
 * silent — Firecracker falls back to BootProtocol::LinuxBoot and enters in long
 * mode at e_entry, i.e. straight into 32-bit trampoline code, and QEMU falls
 * back to a different path entirely. Naming the phdrs here means the note's
 * visibility is stated, not inherited.
 *
 * No KERNEL_VIRT_BASE equivalent yet: this target runs on the identity map that
 * boot.s builds. The aarch64 linker.ld's upper-half split is the next thing to
 * mirror here, and it is deliberately absent rather than half-done.
 */
ENTRY(_start)

PHDRS
{
    note PT_NOTE   FLAGS(4);            /* R   — the PVH note */
    text PT_LOAD    FLAGS(5);           /* R+X */
    rodata PT_LOAD  FLAGS(4);           /* R   */
    data PT_LOAD    FLAGS(6);           /* R+W — .data and .bss */
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
    .boot : { KEEP(*(.text.boot)) } :text
    .text : { *(.text .text.*) } :text

    . = ALIGN(0x1000);
    .rodata : { *(.rodata .rodata.*) } :rodata

    . = ALIGN(0x1000);
    .data : { *(.data .data.*) } :data

    .bss (NOLOAD) : {
        __bss_start = .;
        /* Page tables first and contiguous: boot.s zeroes all three with one
         * `rep stosl` from __pml4, which is only correct while they abut. */
        *(.bss.pagetables)
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    } :data

    _kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
        *(.note.GNU-stack)
        *(.note.gnu.property)
    }
}

ASSERT(_kernel_start == 0x200000, "amd64 image must load at 2 MiB")
