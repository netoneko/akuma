//! The secondary-core trampoline: where PSCI `CPU_ON` starts a core.
//!
//! Zeroes `TPIDRRO_EL0`, enables FP/SIMD, picks the core's 64 KiB stack from
//! `MPIDR_EL1.Aff0`, loads the boot tables `_boot` published
//! (`boot_ttbr0_addr`/`boot_ttbr1_addr`), turns the MMU on with the same
//! `SCTLR_EL1` bits, and calls `secondary_shared_start(context_id, core)`.
//!
//! Moved here from `akuma-entry::smp_shared` (B21 A6, 2026-11-04) unchanged, as
//! `_boot` was in A5.2, so a kernel that is not the main one starts its
//! secondaries through the same instructions. What the linking kernel must
//! provide: `secondary_shared_start(context_id: u64, core: u64) -> !`.

/// Cores the trampoline gives a stack (`Aff0` at or above it parks in `wfe`).
pub const MAX_CORES: usize = 8;

/// Per-core boot/idle stack size shift (64 KiB).
///
/// This stack backs the secondary's idle thread, so it must hold a whole IRQ
/// excursion (the 832-byte trap frame plus the tick and the scheduler). 16 KiB
/// overflowed.
pub const STACK_SHIFT: usize = 16;

unsafe extern "C" {
    /// The trampoline (asm, `.text.boot`, below). Its address is the
    /// identity-mapped PA to hand PSCI `CPU_ON`.
    fn secondary_entry_shared();

    /// The per-core boot/idle stacks (`.bss.smp_shared`, below), declared with
    /// their true type so the size the trampoline walks is stated to the compiler.
    static secondary_boot_stacks_shared: [u8; MAX_CORES << STACK_SHIFT];
}

/// The entry point for PSCI `CPU_ON` (a physical address: the kernel runs
/// identity-mapped).
#[must_use]
pub fn entry_pa() -> u64 {
    secondary_entry_shared as *const () as u64
}

/// Base of `core`'s 64 KiB boot/idle stack. Taking the address of an `extern`
/// static is safe (`&raw const` does not read through it).
#[must_use]
pub fn stack_base(core: usize) -> usize {
    (&raw const secondary_boot_stacks_shared) as usize + (core << STACK_SHIFT)
}

// Secondary trampoline. Mirrors boot.rs's MMU setup, but loads the SHARED boot
// TTBR0/TTBR1 (never a restricted per-core table) and tail-calls
// `secondary_shared_start`. Lives in `.text.boot` so it is identity-reachable with
// the MMU still off at entry. `boot_ttbr0_addr`/`boot_ttbr1_addr` are the BSP's
// boot-table roots, published MMU-off into `.data.boot` by boot.rs.
core::arch::global_asm!(
    r#"
.section .text.boot
.global secondary_entry_shared
secondary_entry_shared:
    mov     x19, x0                 // x19 = context_id (a3 from PSCI CPU_ON; unused M0)

    // 0. Zero TPIDRRO_EL0 before anything can call `current_tid()`.
    //
    // Same reason as the BSP path in `boot.rs`: the register's reset value is
    // architecturally UNKNOWN, and KVM stamps UNKNOWN-reset registers with
    // 0x1de7ec7edbadc0de, which `current_tid()` treats as fatal. Each PSCI-woken
    // secondary gets its own freshly-reset register, so zeroing it on the BSP is
    // not enough.
    msr     tpidrro_el0, xzr

    // 1. Enable FPU/SIMD (FPEN = 0b11).
    mov     x0, #(3 << 20)
    msr     cpacr_el1, x0
    isb

    // 2. core idx = MPIDR aff0; bail to park if it exceeds MAX_CORES.
    mrs     x20, mpidr_el1
    and     x20, x20, #0xff
    cmp     x20, #{max_cores}
    b.ge    .Lsh_park
    // SP = &secondary_boot_stacks_shared[idx] + STACK_SIZE (top of this core's stack)
    adrp    x0, secondary_boot_stacks_shared
    add     x0, x0, :lo12:secondary_boot_stacks_shared
    add     x0, x0, x20, lsl #{stack_shift}
    mov     x1, #1
    add     x0, x0, x1, lsl #{stack_shift}
    mov     sp, x0

    // 3. MMU registers (mirror boot.rs configure_mmu_regs).
    // CNTKCTL_EL1.EL0VCTEN — EL0 reads CNTVCT/CNTFRQ directly (see boot.rs).
    mov     x0, #0x2
    msr     cntkctl_el1, x0
    // MAIR_EL1 = 0xFFBB4400
    mov     x0, #0x4400
    movk    x0, #0xFFBB, lsl #16
    msr     mair_el1, x0
    // TCR_EL1 = 0x0000_0005_B510_3510
    mov     x0, #0x3510
    movk    x0, #0xB510, lsl #16
    movk    x0, #0x5, lsl #32
    msr     tcr_el1, x0
    // TTBR0_EL1 <- *boot_ttbr0_addr (the BSP's SHARED boot table; read MMU-off)
    adrp    x0, boot_ttbr0_addr
    add     x0, x0, :lo12:boot_ttbr0_addr
    ldr     x0, [x0]
    msr     ttbr0_el1, x0
    // TTBR1_EL1 <- *boot_ttbr1_addr
    adrp    x0, boot_ttbr1_addr
    add     x0, x0, :lo12:boot_ttbr1_addr
    ldr     x0, [x0]
    msr     ttbr1_el1, x0
    tlbi    vmalle1
    dsb     sy
    isb
    // Enable MMU + caches (same SCTLR bits as boot.rs _boot_code).
    //
    // Including the two `bic`s: each PSCI-woken core gets its own SCTLR_EL1 with
    // its own reset value, and KVM's has SA/SA0 set while QEMU's does not. Clearing
    // them on the BSP alone would leave every secondary enforcing EL0 SP-alignment
    // checks that this kernel's userspace has never satisfied. See boot.rs.
    mrs     x0, sctlr_el1
    orr     x0, x0, #1              // M  = MMU enable
    orr     x0, x0, #(1 << 2)      // C  = data cache
    orr     x0, x0, #(1 << 12)     // I  = instruction cache
    orr     x0, x0, #(1 << 14)     // DZE
    orr     x0, x0, #(1 << 15)     // UCT
    orr     x0, x0, #(1 << 26)     // UCI
    bic     x0, x0, #(1 << 3)      // SA  = 0: no SP alignment check at EL1
    bic     x0, x0, #(1 << 4)      // SA0 = 0: no SP alignment check at EL0
    msr     sctlr_el1, x0
    isb

    // 4. secondary_shared_start(context_id, core_idx)
    mov     x0, x19
    mov     x1, x20
    bl      secondary_shared_start

.Lsh_park:
    wfe
    b       .Lsh_park

.section .bss.smp_shared
.balign 16
// `.global` because `secondary_stack_base` is pub and gets inlined into callers in
// other codegen units (the boot self-test). Without it the symbol is local to this
// global_asm! block and any out-of-CGU `adrp` reference fails to link.
.global secondary_boot_stacks_shared
secondary_boot_stacks_shared:
    .space  {stacks_bytes}
"#,
    max_cores = const MAX_CORES,
    stack_shift = const STACK_SHIFT,
    stacks_bytes = const (MAX_CORES << STACK_SHIFT),
);
