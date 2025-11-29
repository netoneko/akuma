// IRQ handler registration and dispatch

use alloc::vec::Vec;
use spinning_top::Spinlock;

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
    let handlers = IRQ_HANDLERS.lock();

    if let Some(Some(handler)) = handlers.handlers.get(irq as usize) {
        handler(irq);
    }
}
