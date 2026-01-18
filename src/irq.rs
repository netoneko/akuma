// IRQ handler registration and dispatch

use alloc::vec::Vec;
use spinning_top::Spinlock;

// ============================================================================
// IRQ Guard - RAII guard for disabling interrupts
// ============================================================================

/// RAII guard that disables IRQs when created and restores them when dropped.
/// This ensures IRQs are properly restored even if the guarded code panics.
pub struct IrqGuard {
    saved_daif: u64,
}

impl IrqGuard {
    /// Create a new IRQ guard, disabling IRQs.
    /// The previous IRQ state will be restored when this guard is dropped.
    #[inline]
    pub fn new() -> Self {
        let daif: u64;
        // SAFETY: Reading and modifying DAIF register is safe - it only affects
        // interrupt masking for the current CPU
        unsafe {
            core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack));
            core::arch::asm!("msr daifset, #2", options(nomem, nostack));
            core::arch::asm!("isb", options(nomem, nostack));
        }
        Self { saved_daif: daif }
    }
}

impl Drop for IrqGuard {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: Restoring DAIF register to its previous state is safe
        unsafe {
            core::arch::asm!("msr daif, {}", in(reg) self.saved_daif, options(nomem, nostack));
        }
    }
}

/// Run a closure with IRQs disabled.
/// This is a convenience wrapper around IrqGuard.
#[inline]
pub fn with_irqs_disabled<T, F: FnOnce() -> T>(f: F) -> T {
    let _guard = IrqGuard::new();
    f()
}

/// Disable IRQs. Caller is responsible for re-enabling with enable_irqs().
/// Use with_irqs_disabled() when possible for automatic cleanup.
#[inline]
pub fn disable_irqs() {
    unsafe {
        core::arch::asm!("msr daifset, #2", options(nomem, nostack));
        core::arch::asm!("isb", options(nomem, nostack));
    }
}

/// Enable IRQs. Only call after disable_irqs().
#[inline]
pub fn enable_irqs() {
    unsafe {
        core::arch::asm!("msr daifclr, #2", options(nomem, nostack));
    }
}

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

/// Unregister an IRQ handler
pub fn unregister_handler(irq: u32) {
    let mut handlers = IRQ_HANDLERS.lock();

    if (irq as usize) < handlers.handlers.len() {
        handlers.handlers[irq as usize] = None;
    }

    // Disable the IRQ in GIC
    crate::gic::disable_irq(irq);
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
