//! Boot code for AArch64
//!
//! This module contains the early boot sequence that:
//! 1. Sets up initial page tables for kernel
//! 2. Enables the MMU
//! 3. Continues to Rust kernel main
//!
//! Memory layout:
//! - Kernel runs at physical addresses (0x40000000)
//! - TTBR0: Identity mapping for kernel + user mappings (switched per-process)
//! - TTBR1: Kernel-only mapping (backup, for high address access)

use core::arch::global_asm;

// Physical base address of RAM on QEMU virt (1 GB).
const PHYS_BASE: usize = 0x4000_0000;

// The boot-stack reservation is no longer a hand-tuned per-profile IMAGE_SIZE.
// `linker.ld` derives it from the actual linked image size and exports three
// absolute symbols that auto-track the binary:
//   STACK_TOP     — initial SP, loaded by the asm below (`ldr x0, =STACK_TOP`)
//   STACK_BOTTOM  — first page of the 1 MB boot stack (read by main.rs)
//   IMAGE_RESERVE — load-addr → STACK_BOTTOM byte count, for the ARM64 Image
//                   header field below (QEMU uses it for DTB placement)
// The asm references STACK_TOP / IMAGE_RESERVE directly as external symbols, so
// there is nothing to inject from Rust except PHYS_BASE.

global_asm!(
    r#"
.section .text._boot
.global _boot

// Constants (values injected by Rust at compile time)
.equ KERNEL_PHYS_BASE,  {phys_base}
.equ STACK_SIZE,        0x100000        // 1MB stack
// STACK_TOP and IMAGE_RESERVE are external absolute symbols from linker.ld,
// derived from the actual linked image size (no per-profile constant).

// Page table constants
.equ PAGE_SIZE,         4096

// Page table flags
.equ PT_VALID,          (1 << 0)
.equ PT_TABLE,          (1 << 1)
.equ PT_BLOCK,          (0 << 1)
.equ PT_AF,             (1 << 10)
.equ PT_SH_INNER,       (3 << 8)
.equ PT_SH_OUTER,       (2 << 8)
.equ PT_ATTR_DEVICE,    (0 << 2)        // MAIR index 0 = device
.equ PT_ATTR_NORMAL,    (3 << 2)        // MAIR index 3 = normal WB

// Flags for device memory block (1GB)
.equ DEVICE_BLOCK, (PT_VALID | PT_BLOCK | PT_AF | PT_SH_OUTER | PT_ATTR_DEVICE)
// Flags for normal memory block (1GB)  
.equ NORMAL_BLOCK, (PT_VALID | PT_BLOCK | PT_AF | PT_SH_INNER | PT_ATTR_NORMAL)
// Flags for device L3 page descriptor (PXN | UXN prevent execution)
.equ PT_PXN, (1 << 53)
.equ PT_UXN, (1 << 54)
.equ DEVICE_PAGE, (PT_VALID | PT_TABLE | PT_AF | PT_SH_OUTER | PT_ATTR_DEVICE | PT_PXN | PT_UXN)

_boot:
    // ARM64 Linux Image header (64 bytes).
    // When QEMU detects this header in a flat binary, it:
    //   1. Checks for "ARM\x64" magic at offset 56
    //   2. If magic found AND image_size != 0, loads at RAM_BASE + text_offset
    //   3. If text_offset < 4KB, QEMU adds 2MB to it instead
    //
    // text_offset = 1 MB (0x100000) >= 4 KB so QEMU uses it as-is:
    //   kernel loads at RAM_BASE + 1 MB = 0x40100000.
    //
    // DTB is placed at ALIGN_UP(kernel_load + image_size, 2MB):
    //   ALIGN_UP(0x40100000 + ~0xCB000, 2MB) = 0x40200000
    // This fits DTB in 4 MB RAM with 1 MB to spare.
    //
    // The kernel must be linked at 0x40100000 to match (see linker.ld).
    b       _boot_code          // code0: branch past header
    .word   0                   // code1 (not used)
    .quad   0x100000            // text_offset = 1 MB (QEMU loads at RAM_BASE + 1MB)
    .quad   IMAGE_RESERVE       // image_size: load-addr → boot-stack bottom (linker-derived)
    .quad   0                   // flags: little-endian, 4K pages
    .quad   0                   // res2
    .quad   0                   // res3
    .quad   0                   // res4
    .word   0x644d5241          // magic: "ARM\x64" at offset 56
    .word   0                   // res5

_boot_code:
    // Store x0 (DTB pointer from QEMU) before any modification
    adrp    x1, BOOT_X0_AT_ENTRY
    add     x1, x1, :lo12:BOOT_X0_AT_ENTRY
    str     x0, [x1]
    
    // Save DTB pointer
    mov     x19, x0
    
    // Zero TPIDRRO_EL0 — the current-thread id.
    //
    // `akuma_primitives::preempt::current_tid` reads this register and HALTS the
    // core if it is >= MAX_THREADS, because every per-slot static is indexed by
    // it. Until `threading` installs a real tid, that read has to see 0.
    //
    // The architecture leaves TPIDRRO_EL0's reset value UNKNOWN. QEMU happens to
    // zero it, so relying on that worked for years. KVM does not: its
    // `reset_unknown()` deliberately stamps UNKNOWN-reset system registers with
    // the poison 0x1de7ec7edbadc0de ("I detected bad code") precisely to catch
    // guests that depend on a reset value. Under Firecracker that poison reached
    // `current_tid()` and halted the kernel right after device probing:
    //   [FATAL] TPIDRRO_EL0 CORRUPT: tid=0x1de7ec7edbadc0de >= MAX_THREADS (256)
    // See docs/archive/AKUMA_FIRECRACKER_KVM.md.
    msr     tpidrro_el0, xzr

    // Enable FPU/SIMD
    mov     x0, #(3 << 20)
    msr     cpacr_el1, x0
    isb
    
    // Set up early stack (physical address)
    // Place at top of Code+Stack region (32MB from kernel base)
    // This ensures stack is well above the ~3MB kernel binary
    ldr     x0, =STACK_TOP
    mov     sp, x0
    
    // Zero BSS section (required for flat binary loading - QEMU doesn't
    // zero BSS when loading raw binaries, only when loading ELF)
    adrp    x0, __bss_start
    add     x0, x0, :lo12:__bss_start
    adrp    x1, __bss_end
    add     x1, x1, :lo12:__bss_end
1:  cmp     x0, x1
    b.ge    2f
    str     xzr, [x0], #8
    b       1b
2:
    
    // Set up page tables
    bl      setup_boot_page_tables
    
    // Configure MMU registers
    bl      configure_mmu_regs
    
    // Enable MMU.
    //
    // This reads SCTLR_EL1 and ORs bits in, which means it INHERITS whatever the
    // reset value happened to contain. The architecture leaves several
    // SCTLR_EL1 fields UNKNOWN at reset, and hypervisors disagree:
    //
    //   QEMU virt        SCTLR_EL1 = 0x3490d185   SA=0 SA0=0
    //   Firecracker/KVM  SCTLR_EL1 = 0x34c5d1dd   SA=1 SA0=1
    //
    // So the bits below must be CLEARED explicitly, not merely left unset.
    // Inheriting SA0 under KVM enabled EL0 SP-alignment checking, which this
    // kernel's userspace ABI has never had to satisfy — every `/bin/*` binary
    // took an SP alignment fault (EC=0x26) at its entry point and got SIGILL.
    // See docs/archive/AKUMA_FIRECRACKER_KVM.md.
    //
    // Only the bits Akuma has an opinion about are forced. The rest of the reset
    // value is left alone deliberately: it carries the architecturally RES1
    // fields, and reconstructing those by hand is how you get a subtly wrong
    // SCTLR on the next core revision.
    mrs     x0, sctlr_el1
    orr     x0, x0, #1              // M bit = MMU enable
    orr     x0, x0, #(1 << 2)       // C bit = data cache
    orr     x0, x0, #(1 << 12)      // I bit = instruction cache
    orr     x0, x0, #(1 << 14)      // DZE = EL0 DC ZVA enable (Go runtime uses this for bulk zeroing)
    orr     x0, x0, #(1 << 15)      // UCT = EL0 access to CTR_EL0
    orr     x0, x0, #(1 << 26)      // UCI = EL0 cache maintenance (DC CVAU, IC IVAU)
    bic     x0, x0, #(1 << 3)       // SA  = 0: no SP alignment check at EL1
    bic     x0, x0, #(1 << 4)       // SA0 = 0: no SP alignment check at EL0
    msr     sctlr_el1, x0
    isb
    
    // Continue to Rust (still at physical addresses)
    mov     x0, x19                 // DTB pointer
    bl      rust_start
    
    // Should not return
hang:
    wfe
    b       hang

// Set up boot page tables
// Uses physical addresses since MMU is not yet enabled
.section .text.boot
setup_boot_page_tables:
    // Page tables are in .bss.boot section
    // Use adrp+add for larger range (up to 4GB)
    adrp    x10, boot_page_tables
    add     x10, x10, :lo12:boot_page_tables
    
    // x11 = boot_l0_ttbr0 (for TTBR0, identity mapping)
    mov     x11, x10
    
    // x12 = boot_l0_ttbr1 (for TTBR1, kernel high mapping - not used yet)
    add     x12, x10, #PAGE_SIZE
    
    // x13 = boot_l1 (L1 for TTBR0 identity mapping)
    add     x13, x10, #(PAGE_SIZE * 2)
    
    // x14 = boot_dev_l1 (L1 for device MMIO under L0[1])
    add     x14, x10, #(PAGE_SIZE * 3)
    // x15 = boot_dev_l2 (L2 for device MMIO)
    add     x15, x10, #(PAGE_SIZE * 4)
    // x16 = boot_dev_l3 (L3 for device MMIO pages)
    add     x16, x10, #(PAGE_SIZE * 5)
    
    // Clear page tables (6 pages)
    mov     x0, x10
    mov     x1, #(PAGE_SIZE * 6)
3:  str     xzr, [x0], #8
    subs    x1, x1, #8
    b.ne    3b
    
    // === TTBR0 setup (identity mapping) ===
    // L0[0] -> boot_l1
    mov     x0, x13
    orr     x0, x0, #(PT_VALID | PT_TABLE)
    str     x0, [x11, #0]           // L0[0]
    
    // L1[0] = 0x0000_0000 - 0x3FFF_FFFF (device, 1GB block)
    ldr     x0, =DEVICE_BLOCK
    str     x0, [x13, #0]           // L1[0]
    
    // L1[1] = 0x4000_0000 - 0x7FFF_FFFF.
    //
    // On QEMU virt this is RAM, so it is a Normal-memory block. On Firecracker it
    // is the MMIO32 window — the RTC, the serial port and every virtio slot live
    // in it — so it must be Device. Mapping device registers Normal-cacheable
    // while the same PAs are also mapped Device through L0[1] is a
    // mismatched-attribute alias, which the ARM ARM leaves CONSTRAINED
    // UNPREDICTABLE: the CPU may speculatively read, cache and write back a UART
    // FIFO or a virtio doorbell. The choice comes from
    // `akuma_kernel_core::platform::machine::MMIO_WINDOW_IS_DEVICE`; the flag values stay
    // defined once, here in the assembler, rather than mirrored into Rust.
    ldr     x0, =0x40000000
.if {mmio_window_is_device}
    ldr     x1, =DEVICE_BLOCK
.else
    ldr     x1, =NORMAL_BLOCK
.endif
    orr     x0, x0, x1
    str     x0, [x13, #8]           // L1[1]
    
    // L1[2] = 0x8000_0000 - 0xBFFF_FFFF (more RAM if present)
    ldr     x0, =0x80000000
    ldr     x1, =NORMAL_BLOCK
    orr     x0, x0, x1
    str     x0, [x13, #16]          // L1[2]
    
    // === Device MMIO remapping via L0[1] ===
    // Maps device pages at VA 0x80_0000_0000+ so they don't conflict
    // with user heap in L0[0].
    
    // L0[1] -> boot_dev_l1
    mov     x0, x14
    orr     x0, x0, #(PT_VALID | PT_TABLE)
    str     x0, [x11, #8]           // L0[1]
    
    // boot_dev_l1[0] -> boot_dev_l2
    mov     x0, x15
    orr     x0, x0, #(PT_VALID | PT_TABLE)
    str     x0, [x14, #0]           // dev_l1[0]
    
    // boot_dev_l2[0] -> boot_dev_l3
    mov     x0, x16
    orr     x0, x0, #(PT_VALID | PT_TABLE)
    str     x0, [x15, #0]           // dev_l2[0]
    
    // Device page entries in boot_dev_l3.
    //
    // ONLY the UART is mapped here. That is deliberate and is the whole point of
    // the platform split: this assembly runs before the MMU is fully configured
    // and long before any FDT can be parsed, so every address it uses has to be
    // a compile-time literal — and compile-time literals cannot describe
    // Firecracker's GIC, whose redistributor base moves with the configured vCPU
    // count (proposals/FIRECRACKER_PORT.md §2.1).
    //
    // So the boot table maps exactly enough to make `safe_print!` work, and Rust
    // installs the real device map from the FDT via
    // `mmu::rebuild_boot_device_table` before the first GIC or virtio access.
    // The previous version of this block hardcoded seven QEMU-virt physical
    // addresses here and a mirrored copy in `akuma-exec`'s `DEV_PAGES`.
    ldr     x1, =DEVICE_PAGE

    ldr     x0, ={uart_pa}
    orr     x0, x0, x1
    str     x0, [x16, #({uart_slot} * 8)]   // UART PL011 — the console

    // Store TTBR0 address
    adrp    x0, boot_ttbr0_addr
    add     x0, x0, :lo12:boot_ttbr0_addr
    str     x11, [x0]
    
    // For now, TTBR1 points to same tables (kernel can use either range)
    // Later we can set up proper high-address kernel mapping
    adrp    x0, boot_ttbr1_addr
    add     x0, x0, :lo12:boot_ttbr1_addr
    str     x11, [x0]
    
    ret

// Configure MMU control registers
configure_mmu_regs:
    // CNTKCTL_EL1 - let EL0 read the virtual counter + frequency directly
    // (EL0VCTEN, bit 1). Without this every userspace `mrs cntvct_el0` /
    // `mrs cntfrq_el0` traps to EL1 (EC=0x18) — measured at ~1M pairs/s under
    // llama.cpp decode, and the emulation returned 0 for both, freezing
    // userspace's hardware clock (docs/archive/CROSS_CORE_THREAD_COLLAPSE.md
    // §3). Linux enables this unconditionally.
    mov     x0, #0x2
    msr     cntkctl_el1, x0

    // MAIR_EL1 - Memory Attribute Indirection Register
    // Attr0: Device-nGnRnE (0x00)
    // Attr1: Normal Non-cacheable (0x44)
    // Attr2: Normal Write-through (0xBB)
    // Attr3: Normal Write-back (0xFF)
    mov     x0, #0x4400
    movk    x0, #0xFFBB, lsl #16
    msr     mair_el1, x0
    
    // TCR_EL1 - Translation Control Register
    // T0SZ = 16, T1SZ = 16 (48-bit VA)
    // TG0 = 0 (4KB), TG1 = 2 (4KB)
    // IPS = 5 (48-bit PA)
    // SH0 = SH1 = 3 (Inner shareable)
    // ORGN/IRGN = 1 (Write-back)
    mov     x0, #0x3510
    movk    x0, #0xB510, lsl #16
    movk    x0, #0x5, lsl #32
    msr     tcr_el1, x0
    
    // Load page table addresses
    adrp    x0, boot_ttbr0_addr
    add     x0, x0, :lo12:boot_ttbr0_addr
    ldr     x0, [x0]
    msr     ttbr0_el1, x0
    
    adrp    x0, boot_ttbr1_addr
    add     x0, x0, :lo12:boot_ttbr1_addr
    ldr     x0, [x0]
    msr     ttbr1_el1, x0
    
    // Invalidate TLB
    tlbi    vmalle1
    dsb     sy
    isb
    
    ret

// Data section for boot
.section .data.boot
.balign 8
.global boot_ttbr0_addr
boot_ttbr0_addr:
    .quad   0
.global boot_ttbr1_addr
boot_ttbr1_addr:
    .quad   0
// Reserve space for boot page tables (6 pages = 24KB)
// Pages 0-2: L0 TTBR0, L0 TTBR1, L1 (identity mapping)
// Pages 3-5: L1, L2, L3 for device MMIO under L0[1]
// Must be 4KB aligned
.section .bss.boot
.balign 4096
.global boot_page_tables
boot_page_tables:
    .space  4096 * 6
"#,
    phys_base = const PHYS_BASE,
    uart_pa = const akuma_kernel_core::platform::machine::UART_PA,
    uart_slot = const UART_L3_SLOT,
    mmio_window_is_device = const (akuma_kernel_core::platform::machine::MMIO_WINDOW_IS_DEVICE as usize),
);

/// L3 slot the console UART occupies in the L0[1] device window.
///
/// Derived from the VA layout rather than written down twice — the boot assembly
/// and `akuma_exec::mmu` must agree on it, and a mirrored constant is how that
/// kind of pair drifts.
const UART_L3_SLOT: usize =
    (akuma_primitives::addr::DEV_UART_VA - akuma_primitives::addr::DEV_WINDOW_VA) / 4096;

/// `x0` as the firmware/QEMU left it at the very first instruction of `_boot`,
/// before anything could modify it — the DTB pointer, on every platform that
/// passes one.
///
/// The storage is a Rust static rather than a `.quad` in the assembly above so
/// that reading it needs no `unsafe`: the boot code's `str x0, [x1]` is a plain
/// aligned 64-bit store, which a relaxed atomic load is entitled to observe. It
/// lives in `.data.boot` deliberately — the store happens at `_boot + 4`, long
/// before the `.bss` clear at the top of `_boot_code`, so a `.bss` home would
/// see the value zeroed back out.
#[unsafe(no_mangle)]
#[unsafe(link_section = ".data.boot")]
pub static BOOT_X0_AT_ENTRY: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Read [`BOOT_X0_AT_ENTRY`]. Single writer, in assembly, before Rust runs.
#[must_use]
pub fn x0_at_entry() -> u64 {
    BOOT_X0_AT_ENTRY.load(core::sync::atomic::Ordering::Relaxed)
}
