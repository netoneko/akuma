// IRQ handler registration and dispatch

use alloc::vec::Vec;
use spinning_top::Spinlock;

// ============================================================================
// IRQ Guard - RAII guard for disabling interrupts
// ============================================================================

/// RAII guard that masks IRQs when created and restores `DAIF` when dropped, so
/// IRQs come back even if the guarded code unwinds.
///
/// Re-exported from `akuma_primitives::irq`. This crate and `akuma-exec` each
/// carried an `IrqGuard` under the same name — that shared name is why the
/// duplicate stayed invisible to a grep — and `akuma-exec/src/sync.rs` carried a
/// third, barrier-less DAIF implementation beside them. One now, with the `isb`
/// difference documented and preserved.
pub use akuma_primitives::irq::with_irqs_disabled;

/// Unbalanced IRQ mask/unmask, re-exported under this crate's historical names.
///
/// Both had **zero callers** at the time `akuma-primitives` absorbed the DAIF
/// code — the only greps were their own doc comments and two comments elsewhere
/// referring to them — while six other sites open-coded exactly what
/// `enable_irqs` does. Two of those six were in `akuma-exec`, which cannot reach
/// this module: the missing-crate shape again.
///
/// Prefer `with_irqs_disabled()` / `IrqGuard` — these leave the caller
/// responsible for the matching call and cannot restore a prior masked state.
pub use akuma_primitives::irq::{mask_irqs_sync as disable_irqs, unmask_irqs as enable_irqs};

// ============================================================================
// IRQ Handler Registration
// ============================================================================

type IrqHandler = fn(u32);

struct IrqHandlers {
    handlers: Vec<Option<IrqHandler>>,
}

static IRQ_HANDLERS: Spinlock<IrqHandlers> = Spinlock::new(IrqHandlers {
    handlers: Vec::new(),
});

/// Register an IRQ handler
pub fn register_handler(irq: u32, handler: IrqHandler) {
    let mut handlers = IRQ_HANDLERS.lock();

    // Ensure the handlers vector is large enough
    while handlers.handlers.len() <= irq as usize {
        handlers.handlers.push(None);
    }

    handlers.handlers[irq as usize] = Some(handler);

    // Enable the IRQ in GIC
    crate::gic::enable_irq(irq);
}

/// Dispatch an IRQ to its registered handler
pub fn dispatch_irq(irq: u32) {
    // Copy the handler out while holding the lock, then call it without the lock
    // This prevents deadlocks if the handler needs to register/unregister handlers
    let handler = {
        let handlers = IRQ_HANDLERS.lock();
        handlers.handlers.get(irq as usize).copied().flatten()
    };

    if let Some(handler) = handler {
        handler(irq);
    }
}
