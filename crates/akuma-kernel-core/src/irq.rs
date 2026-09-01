// IRQ handler registration and dispatch

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

/// INTIDs this kernel can register a handler for. The GIC's SPI space ends at
/// 1020, but every registration in the tree is either the timer PPI (27) or a
/// virtio-mmio SPI (`VIRTIO_INTID_BASE` 48 + slot, 32 slots on qemu-virt), so
/// 256 covers the space with room to spare at a cost of 2 KB of `.bss`.
const MAX_IRQ: usize = 256;

struct IrqHandlers {
    /// Fixed table, **not** a `Vec`. This is an IRQ-context data structure:
    /// `dispatch_irq` takes the lock below from the interrupt vector, so
    /// anything that can allocate — or merely take a while — while the lock is
    /// held is a deadlock waiting for the right interrupt. The `Vec` this
    /// replaced grew by `push`ing `None` one entry at a time until it reached
    /// the requested INTID, i.e. up to 49 heap allocations **inside** that
    /// lock, on the boot path, with the BKL held.
    handlers: [Option<IrqHandler>; MAX_IRQ],
}

static IRQ_HANDLERS: Spinlock<IrqHandlers> = Spinlock::new(IrqHandlers {
    handlers: [None; MAX_IRQ],
});

/// Register an IRQ handler
pub fn register_handler(irq: u32, handler: IrqHandler) {
    let idx = irq as usize;
    if idx >= MAX_IRQ {
        // Out of range for the table. Match `akuma_gic::enable_irq`'s posture for an
        // invalid INTID (ignore) rather than enabling a line nothing can service.
        return;
    }

    // Publish the handler with IRQs masked and the lock dropped BEFORE the line
    // is enabled. Both halves matter, and both were wrong:
    //
    // 1. `enable_irq` used to run while this lock was held. `dispatch_irq` takes
    //    the same NON-reentrant spinlock from the interrupt vector, so if the
    //    newly-enabled line delivered on this core before the guard dropped, the
    //    core deadlocked against itself — while holding the BKL, which promotes a
    //    one-core stall into the `[BKL] stuck: owner=N` storm every other core
    //    piles into. Enabling after the guard drops closes that window.
    // 2. Masking keeps the hold atomic against this core's own interrupts, so an
    //    ALREADY-enabled line (the timer PPI is registered twice — probe handler,
    //    then the real one) cannot re-enter the lock mid-update either.
    with_irqs_disabled(|| {
        IRQ_HANDLERS.lock().handlers[idx] = Some(handler);
    });

    // Outside the lock: this can deliver `irq` on this core immediately.
    akuma_gic::enable_irq(irq);
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
