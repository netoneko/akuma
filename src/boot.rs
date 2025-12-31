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

// Kernel physical base address
pub const KERNEL_PHYS_BASE: usize = 0x4000_0000;

global_asm!(
    r#"
.section .text._boot
.global _boot

// Constants
.equ KERNEL_PHYS_BASE,  0x40000000
.equ STACK_SIZE,        0x100000        // 1MB stack

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

_boot:
    // Save DTB pointer
    mov     x19, x0
    
    // Enable FPU/SIMD
    mov     x0, #(3 << 20)
    msr     cpacr_el1, x0
    isb
    
    // Set up early stack (physical address)
    ldr     x0, =KERNEL_PHYS_BASE
    add     x0, x0, #STACK_SIZE
    mov     sp, x0
    
    // Set up page tables
    bl      setup_boot_page_tables
    
    // Configure MMU registers
    bl      configure_mmu_regs
    
    // Enable MMU
    mrs     x0, sctlr_el1
    orr     x0, x0, #1              // M bit = MMU enable
    orr     x0, x0, #(1 << 2)       // C bit = data cache
    orr     x0, x0, #(1 << 12)      // I bit = instruction cache
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
    
    // Clear page tables (3 pages)
    mov     x0, x10
    mov     x1, #(PAGE_SIZE * 3)
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
    
    // L1[1] = 0x4000_0000 - 0x7FFF_FFFF (RAM, 1GB block)
    ldr     x0, =0x40000000
    ldr     x1, =NORMAL_BLOCK
    orr     x0, x0, x1
    str     x0, [x13, #8]           // L1[1]
    
    // L1[2] = 0x8000_0000 - 0xBFFF_FFFF (more RAM if present)
    ldr     x0, =0x80000000
    ldr     x1, =NORMAL_BLOCK
    orr     x0, x0, x1
    str     x0, [x13, #16]          // L1[2]
    
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

// Reserve space for boot page tables (3 pages = 12KB)
// Must be 4KB aligned
.section .bss.boot
.balign 4096
.global boot_page_tables
boot_page_tables:
    .space  4096 * 3
"#
);
