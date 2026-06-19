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

use akuma_exec::runtime::{config, runtime, with_irqs_disabled, IrqGuard};
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
        "test precondition: IRQs should be enabled before guard (daif={before:#x})"
    );

    {
        let _g = IrqGuard::new();
        let during = read_daif();
        assert!(
            during & DAIF_I_BIT != 0,
            "IrqGuard must mask the I-bit (daif={during:#x})"
        );
    }

    let after = read_daif();
    assert_eq!(
        after, before,
        "IrqGuard Drop must restore DAIF to pre-guard state ({after:#x} != {before:#x})"
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
    let observed = with_irqs_disabled(read_daif);
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
        "yield_now under IrqGuard must increment YIELD_WITH_IRQS_MASKED (was {initial}, now {after})"
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

fn test_runtime_is_lock_free_under_masked_irqs() {
    // Regression test for the hang found at uptime ~217s on instance 7
    // of the 2026-05-28 parallel hunt (see docs/STABILITY_URGENT_ISSUES.md
    // issue #1). Root cause: `akuma_exec::runtime::RUNTIME` and `CONFIG`
    // were `Spinlock<Option<T>>`. The timer IRQ handler called
    // `runtime().uptime_us()` from `check_preemption_watchdog`; if EL1
    // code held the same lock when the IRQ fired, the IRQ-side acquire
    // self-deadlocked on this single-CPU kernel.
    //
    // After the fix (lock-free `OnceCopy<T>`), `runtime()` / `config()`
    // must be valid IRQ-context calls. We can't synthesise an
    // EL1-held-the-lock-then-IRQ scenario from a kernel thread, but we
    // can drive thousands of calls under DAIF.I=1 — pre-fix this would
    // already deadlock the first time any other thread held the lock;
    // post-fix it is uniformly fast and DAIF is restored cleanly.

    let saved_yield_count = YIELD_WITH_IRQS_MASKED.load(Ordering::Relaxed);

    with_irqs_disabled(|| {
        let during = read_daif();
        assert!(
            during & DAIF_I_BIT != 0,
            "precondition: IRQs masked inside the closure (daif={during:#x})"
        );

        // 10k iterations of the exact call shape the watchdog uses.
        // If the underlying storage ever regresses to a Spinlock and any
        // other thread holds it, this loop hangs the test boot —
        // converting a silent-stall regression into a loud failure.
        for _ in 0..10_000 {
            let rt = runtime();
            // Touch a field so the compiler can't elide the load.
            let _ = (rt.uptime_us)();
            let cfg = config();
            // Touch a config field for the same reason.
            assert!(cfg.max_threads > 0, "config().max_threads must be > 0");
        }
    });

    let after = read_daif();
    assert!(
        after & DAIF_I_BIT == 0,
        "DAIF.I must be cleared after with_irqs_disabled (daif={after:#x})"
    );

    // Sanity: this path must not yield (we never call yield_now), so the
    // masked-yield counter is untouched.
    assert_eq!(
        YIELD_WITH_IRQS_MASKED.load(Ordering::Relaxed),
        saved_yield_count,
        "runtime()/config() reads must not yield"
    );

    console::print("  [PASS] test_runtime_is_lock_free_under_masked_irqs\n");
}

pub fn run_all_tests() {
    console::print("\n--- DAIF / IRQ-mask Tests ---\n");
    test_irq_guard_masks_and_restores();
    test_nested_irq_guard_preserves_outer();
    test_with_irqs_disabled_matches_guard();
    test_yield_now_clean_path_does_not_warn();
    test_yield_now_detects_masked_yield();
    test_runtime_is_lock_free_under_masked_irqs();
    console::print("--- DAIF tests complete ---\n");
}
