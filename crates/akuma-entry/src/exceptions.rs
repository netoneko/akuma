//! Link-level pieces the hooked front end (`crate::hooked`) needs.
//!
//! The EL1 fault recovery pad, the exception-stack register write (`TPIDR_EL1`), and the
//! crash-dump and IRQ-frame reads `akuma-exceptions` cannot make itself, handed over as
//! plain `fn` pointers and only ever called on addresses that crate derived from live
//! exception state.
//!
//! Moved out of `akuma-exceptions` (2026-11-06, `docs/archive/UNSAFE_FORBID_THREE_CRATES_PLAN.md`)
//! so that crate can `forbid(unsafe_code)`. The SGI scheduler's vector table and its
//! `#[no_mangle]` handlers lived here until the hooked scheduler replaced it (2026-11-11).

use akuma_exceptions as exc;
use akuma_trap_aarch64::irq_frame::IrqFrame;
#[cfg(kernel_tests)]
use core::arch::global_asm;

// ============================================================================
// Handlers
// ============================================================================

/// The landing pad an unrecoverable EL1 data abort in a dying process's kernel code
/// `eret`s to; see `akuma_exceptions::sync_el1`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn el1_fault_recovery_pad() {
    exc::el1_fault_recovery();
}

// ============================================================================
// Services handed to the logic crate
// ============================================================================

/// The live IRQ trap frame at `sp`. Only ever called by `akuma_exceptions` with the SP
/// of the frame the IRQ asm is about to restore, or the one it just built.
fn irq_frame_at(sp: u64) -> IrqFrame {
    // SAFETY: see the doc comment - `sp` is a live IRQ trap frame (the scheduler hands
    // back the next thread's saved frame, or the interrupted one).
    unsafe { IrqFrame::at(sp) }
}

/// Display-only read of kernel memory in the fatal EL1 crash dump.
fn peek_u32(addr: u64) -> u32 {
    // SAFETY: the dump calls this only for an address in the kernel identity range
    // taken from ELR_EL1; display-only, and a fault in the crash path is already fatal.
    unsafe { *(addr as *const u32) }
}

/// As [`peek_u32`], for the page-table words the walk-fault dump prints.
fn peek_u64(addr: u64) -> u64 {
    // SAFETY: as [`peek_u32`]; the addresses come from TTBR0 / the L0 entry just read.
    unsafe { *(addr as *const u64) }
}

/// Copy the physical frame `pa` (a PMM / file-cache frame) into `out`.
fn read_frame_bytes(pa: usize, out: &mut [u8]) {
    // SAFETY: `pa` is a frame of RAM that no Rust object lives in: the file-page cache
    // hands out PMM frames and the verify diagnostic is its only reader here.
    unsafe { akuma_phys::Frame::from_pa(akuma_phys::PhysAddr::from_usize(pa)) }.read_bytes(0, out);
}

/// Set the current exception stack for the running thread
/// Called during context switch to update TPIDR_EL1
#[inline]
pub fn set_current_exception_stack(stack_top: u64) {
    // Bare metal only: `msr tpidr_el1` is an EL1 instruction. It *assembles* on
    // the aarch64 development host, so an ungated body would build under
    // `cargo test` and die with `SIGILL` the first time a host test reached it —
    // the trap `akuma-cpu`'s "Host builds" note records. See this crate's own
    // note of the same name.
        unsafe {
        core::arch::asm!("msr tpidr_el1, {}", in(reg) stack_top);
    }
}

/// Initialize the exception stack pointer for the boot thread
/// Must be called before any user mode code runs
pub fn init_exception_stack() {
    // Boot thread (thread 0) uses the boot stack; its early exception stack is at
    // the very top of that region. Read the STACK_TOP linker symbol (derived from
    // the linked image size in linker.ld, the same one boot.rs loads as the initial
    // SP and main.rs reads) so this can never go stale. Hardcoding the old
    // 0x40800000 once pointed the early exception stack into the kernel heap at low
    // RAM after the boot stack was relocated — see docs/LOW_MEMORY_ENVIRONMENT.md
    // "Known bug".
    unsafe extern "C" {
        static STACK_TOP: u8;
    }
    let boot_stack_top = &raw const STACK_TOP as u64;
    set_current_exception_stack(boot_stack_top);
}

/// Hand `akuma-exceptions` the crash-dump and frame reads it cannot make itself;
/// `crate::hooked::init` registers them.
pub(crate) fn register_services() {
    exc::register_entry_services(exc::EntryServices {
        irq_frame_at,
        peek_u32,
        peek_u64,
        read_frame_bytes,
    });
}

// ---------------------------------------------------------------------------
// Regression probe: is the EL1 sync exception transparent to x4-x18?
//
// Gated on `kernel_tests` like `src/process_tests.rs`, its only caller: the
// extreme-size profile drops the boot suite, and a probe that provokes a data
// abort has no business in a build that will never assert on it.
// ---------------------------------------------------------------------------

#[cfg(kernel_tests)]
unsafe extern "C" {
    fn __el1_gpr_transparency_probe(out: *mut u64, base: u64) -> u64;
}

#[cfg(kernel_tests)]
global_asm!(
    r#"
.text
.global __el1_gpr_transparency_probe
.global __el1_gpr_transparency_landing

// x0 = out (15 u64 slots), x1 = pattern base.
// Loads `base + N` into xN for N in 4..=18, takes ONE EL1 data abort, and writes
// what those registers hold *after* the exception into `out`. The caller compares.
//
// Leaf and stackless on purpose, for the same reason __arch_copy_user_memory is:
// the recovery rewrites ELR_EL1 to the landing label below, so anything this
// function pushed would never be popped. It also keeps the live pattern out of
// x0-x3/x29/x30, which the vector has always saved — those would pass even with
// the bug.
__el1_gpr_transparency_probe:
    add     x4,  x1, #4
    add     x5,  x1, #5
    add     x6,  x1, #6
    add     x7,  x1, #7
    add     x8,  x1, #8
    add     x9,  x1, #9
    add     x10, x1, #10
    add     x11, x1, #11
    add     x12, x1, #12
    add     x13, x1, #13
    add     x14, x1, #14
    add     x15, x1, #15
    add     x16, x1, #16
    add     x17, x1, #17
    add     x18, x1, #18

    mov     x2, x0                  // out pointer: x2 is saved by the vector
    // A high user VA nothing maps or lazily backs. NOT a low one like 0x1000:
    // measured 2026-08-28, a store there did not fault during the boot suite, so
    // the first version of this test passed vacuously on a kernel with the bug.
    movz    x3, #0x6000, lsl #32    // 0x0000_6000_0000_0000
    str     x4, [x3]                // <-- the abort. ELR is rewritten to 1f, or
                                    //     a resolved fault retries this store.
__el1_gpr_transparency_landing:
1:  stp     x4,  x5,  [x2, #0]
    stp     x6,  x7,  [x2, #16]
    stp     x8,  x9,  [x2, #32]
    stp     x10, x11, [x2, #48]
    stp     x12, x13, [x2, #64]
    stp     x14, x15, [x2, #80]
    stp     x16, x17, [x2, #96]
    str     x18,      [x2, #112]
    mov     x0, #0
    ret
    "#
);

/// Which of x4..x18 an EL1 sync exception failed to preserve — a bitmask where
/// bit `n - 4` means register `xn` came back wrong. `0` is the healthy answer.
///
/// The property under test is that `sync_el1_handler` is transparent to every
/// GPR, because `try_resolve_el1_cow_fault` and
/// `try_resolve_el1_user_copy_lazy_fault` resolve the fault and `eret` back to
/// **re-execute the faulting instruction**. A retry is only correct if that
/// instruction's inputs survived, so a register the handler clobbers is stored
/// into memory by the retry as if it were data.
///
/// The vector used to save x0-x3/x29/x30 only. That was enough while
/// `__arch_copy_user_memory` was a byte loop holding its one live datum in x3;
/// the 2026-08-27 widening put live data in x3-x10, and a `read(2)` whose
/// destination page faulted in mid-copy then stored leftover handler state —
/// 24 bytes per page, with `read` reporting the full count
/// (`docs/archive/BUSYBOX_HASH_MISCOMPUTE.md`).
///
/// Runs on whichever recovery arm the fault takes: the resolve-and-retry paths if
/// the VA turns out to be lazily mappable, otherwise the registered user-copy
/// trampoline. Both return through this same vector, so either pins the fix.
///
/// Returns `(clobber_mask, aborts_taken)`. **`aborts_taken` is not decoration** —
/// if VA `0x1000` were mapped in the current address space the store would simply
/// succeed, no exception would fire, and the mask would read `0` for a kernel with
/// the bug wide open. A test that cannot fail is worse than no test
/// (`docs/archive/SMP_ADOPTED_IDLE_STACK_CLOBBER.md` is the local precedent), so
/// the caller must require that the count moved.
#[must_use]
#[cfg(kernel_tests)]
pub fn el1_sync_gpr_clobber_mask() -> (u32, u64) {
    const BASE: u64 = 0x5A5A_0000;
    const EC_DATA_ABORT_EL1: usize = 0x25;
    let mut out = [0u64; 15];

    let before = exc::SYNC_EC_EL1[EC_DATA_ABORT_EL1].load(core::sync::atomic::Ordering::Relaxed);

    // The trampoline arm needs a registered handler, or an unrecoverable EL1
    // abort would kill the current process instead of landing at the label.
    akuma_exec::threading::set_user_copy_fault_handler(
        __el1_gpr_transparency_landing as *const () as usize as u64,
    );
    // SAFETY: `out` has the 15 slots the probe writes, and the abort it takes is
    // recovered by the handler registered just above.
    unsafe { __el1_gpr_transparency_probe(out.as_mut_ptr(), BASE) };
    akuma_exec::threading::set_user_copy_fault_handler(0);

    let after = exc::SYNC_EC_EL1[EC_DATA_ABORT_EL1].load(core::sync::atomic::Ordering::Relaxed);

    let mut mask = 0u32;
    for (i, got) in out.iter().enumerate() {
        if *got != BASE + (i as u64 + 4) {
            mask |= 1 << i;
        }
    }
    (mask, after.wrapping_sub(before))
}

#[cfg(kernel_tests)]
unsafe extern "C" {
    fn __el1_gpr_transparency_landing();
}
