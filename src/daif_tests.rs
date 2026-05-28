//! DAIF / IRQ-mask correctness tests.
//!
//! Covers the invariants that protect the kernel from the silent-hang
//! failure mode described in `docs/STABILITY_URGENT_ISSUES.md` issue #1:
//! a thread that yields with IRQs masked silently busy-spins (the SGI
//! is gated by DAIF.I), and on this single-core kernel that wedges the
//! timer interrupt too — both heartbeats stop together.
//!
//! These tests verify:
//!   1. `IrqGuard` masks and then restores the I-bit correctly.
//!   2. Nested guards preserve the outer scope's mask state.
//!   3. `with_irqs_disabled` matches `IrqGuard` semantics.
//!   4. `yield_now()` detects and counts yields issued under a mask.

use core::sync::atomic::Ordering;

use akuma_exec::runtime::{with_irqs_disabled, IrqGuard};
use akuma_exec::threading::{yield_now, YIELD_WITH_IRQS_MASKED};

use crate::console;

const DAIF_I_BIT: u64 = 1 << 7;

#[inline]
fn read_daif() -> u64 {
    let daif: u64;
    unsafe {
        core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack));
    }
    daif
}

fn test_irq_guard_masks_and_restores() {
    let before = read_daif();
    assert!(
        before & DAIF_I_BIT == 0,
        "test precondition: IRQs should be enabled before guard (daif={:#x})",
        before
    );

    {
        let _g = IrqGuard::new();
        let during = read_daif();
        assert!(
            during & DAIF_I_BIT != 0,
            "IrqGuard must mask the I-bit (daif={:#x})",
            during
        );
    }

    let after = read_daif();
    assert_eq!(
        after, before,
        "IrqGuard Drop must restore DAIF to pre-guard state ({:#x} != {:#x})",
        after, before
    );
    console::print("  [PASS] test_irq_guard_masks_and_restores\n");
}

fn test_nested_irq_guard_preserves_outer() {
    let before = read_daif();
    {
        let _outer = IrqGuard::new();
        let outer_state = read_daif();
        assert!(outer_state & DAIF_I_BIT != 0, "outer must mask");

        {
            let _inner = IrqGuard::new();
            assert!(read_daif() & DAIF_I_BIT != 0, "inner must keep mask");
        }

        // Outer is still in scope; mask must still be set.
        assert!(
            read_daif() & DAIF_I_BIT != 0,
            "inner Drop must NOT clear mask while outer still in scope"
        );
    }
    assert_eq!(
        read_daif(),
        before,
        "outer Drop must restore pre-guard state"
    );
    console::print("  [PASS] test_nested_irq_guard_preserves_outer\n");
}

fn test_with_irqs_disabled_matches_guard() {
    let before = read_daif();
    let observed = with_irqs_disabled(|| read_daif());
    let after = read_daif();

    assert!(
        observed & DAIF_I_BIT != 0,
        "with_irqs_disabled body must run with I-bit set"
    );
    assert_eq!(
        after, before,
        "with_irqs_disabled must restore DAIF on return"
    );
    console::print("  [PASS] test_with_irqs_disabled_matches_guard\n");
}

fn test_yield_now_detects_masked_yield() {
    let initial = YIELD_WITH_IRQS_MASKED.load(Ordering::Relaxed);

    with_irqs_disabled(|| {
        // SGI cannot deliver while DAIF.I=1, so this is structurally the
        // failure-mode call. The instrumentation must count it.
        yield_now();
    });

    let after = YIELD_WITH_IRQS_MASKED.load(Ordering::Relaxed);
    assert!(
        after > initial,
        "yield_now under IrqGuard must increment YIELD_WITH_IRQS_MASKED (was {}, now {})",
        initial,
        after
    );
    console::print("  [PASS] test_yield_now_detects_masked_yield\n");
}

fn test_yield_now_clean_path_does_not_warn() {
    // With IRQs enabled, yield_now is the normal path and must NOT bump
    // the masked-yield counter.
    let before = read_daif();
    assert!(before & DAIF_I_BIT == 0, "precondition: IRQs enabled");

    let initial = YIELD_WITH_IRQS_MASKED.load(Ordering::Relaxed);
    yield_now();
    let after = YIELD_WITH_IRQS_MASKED.load(Ordering::Relaxed);

    assert_eq!(
        after, initial,
        "yield_now with IRQs enabled must not increment masked counter"
    );
    console::print("  [PASS] test_yield_now_clean_path_does_not_warn\n");
}

pub fn run_all_tests() {
    console::print("\n--- DAIF / IRQ-mask Tests ---\n");
    test_irq_guard_masks_and_restores();
    test_nested_irq_guard_preserves_outer();
    test_with_irqs_disabled_matches_guard();
    test_yield_now_clean_path_does_not_warn();
    test_yield_now_detects_masked_yield();
    console::print("--- DAIF tests complete ---\n");
}
