/* Link script for the guest programs the amd64 kernel loads
 * (the per-program directories under `userspace/amd64`, built by `amd64/build.rs`).
 *
 * 0x40_0000 is where a static Linux x86_64 binary is linked, and since Stage K
 * the kernel is in the upper half — so the program is mapped where it expects
 * to be rather than at an address chosen to dodge the kernel.
 *
 * Every section boundary is `ALIGN(0x1000)`, which is what puts each of
 * .text/.rodata/.data+.bss in its own PT_LOAD *and* its own page. Without it
 * lld packs them into one page, and then a single page has to carry the union
 * of three permission sets — an executable, writable page. The kernel's loader
 * refuses that mapping (see `amd64/src/loader.rs`), so the alignment here is
 * load-bearing rather than cosmetic: it is what real static binaries do, and
 * what makes W^X expressible at all.
 *
 * `.got` is named explicitly for the reason the kernel's own script does
 * (`docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.17.2): a section the script
 * does not mention is still emitted, and where it lands is not the author's
 * decision. `relocation-model=static` leaves it nearly empty, not absent.
 */
ENTRY(_start)

SECTIONS {
    . = 0x400000;

    .text : { *(.text._start) *(.text .text.*) }

    . = ALIGN(0x1000);
    .rodata : { *(.rodata .rodata.*) }

    . = ALIGN(0x1000);
    .data : {
        *(.got .got.plt)
        *(.data .data.*)
    }
    /* No ALIGN before .bss: it shares the writable segment with .data on
     * purpose, so the image has a PT_LOAD whose p_memsz exceeds its p_filesz —
     * the case the loader's zero-fill exists for. */
    .bss : { *(.bss .bss.*) *(COMMON) }

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
        *(.note.*)
    }
}
