# IRQ dispatch

IRQ masking and the handler-registration/dispatch table above the GIC.
Source: `src/irq.rs`. For the GIC backend itself (GICv2/v3 selection, the
exception-vector integration that calls into this file, and the SMP
doorbell) see [`drivers/gic.md`](drivers/gic.md) "IRQ dispatch to the
scheduler" — not duplicated here.

> **Stability: B (watch).** Dormant since Dec 2025. Not part of either fire
> window's bug churn.

## IRQ masking

`IrqGuard` (`:12-41`) is an RAII guard: `IrqGuard::new()` reads `DAIF`, sets
the I-bit (`daifset, #2`), and `isb`s; `Drop` restores the saved `DAIF`
value. `with_irqs_disabled<T>(f: FnOnce() -> T) -> T` (`:46-49`) is the
closure-based wrapper — this is the form almost everything in the kernel
uses (e.g. [`console.rs`](console.md)'s UART path), since it can't forget to
re-enable even on panic/early-return. `disable_irqs()`/`enable_irqs()`
(`:54-67`) are the raw, non-guarded primitives for callers that need to
straddle IRQ state across a boundary the guard's stack discipline can't
express.

## Handler registration and dispatch

A single global table, `IRQ_HANDLERS: Spinlock<IrqHandlers>` (`:79-81`), maps
`irq: u32` to `fn(u32)`. `register_handler(irq, handler)` (`:84-96`) grows the
`Vec` on demand, installs the handler, and calls `gic::enable_irq(irq)` to
unmask it at the controller. `dispatch_irq(irq)` (`:115-126`) copies the
handler out from under the lock before calling it — holding the spinlock
across the handler call would deadlock any handler that itself needs to
register/unregister (e.g. during bring-up).

See [`drivers/gic.md`](drivers/gic.md) for how the top-level exception vector
special-cases the scheduler SGI versus routing everything else through
`dispatch_irq`.

## Background

- `docs/archive/STRATEGY_C_IRQ_WAKEUPS.md` — an early plan for moving I/O
  off polling loops onto IRQ-driven wakeups; background/context only, not a
  description of the current `irq.rs` implementation.
