# Console (UART) I/O

PL011 UART driver and the kernel's console output/input chokepoint. Source:
`src/console.rs`.

> **Stability: B (watch).** Dormant since March 2026 (`extract akuma-exec`)
> except one June 2026 multikernel change (`use console ring`, Jun 29) that
> rerouted all output through a new `emit()` chokepoint. The change is
> additive and mechanical — every print helper now funnels through one
> function instead of writing the UART directly — but recent enough to watch.

## UART driver

`Uart` (`:19-57`) wraps the PL011 registers at `akuma_exec::mmu::DEV_UART_VA`
(remapped VA for physical `0x0900_0000`): `write`/`read` hit the data
register (`DR_OFFSET`), `has_data()` checks the flag register's `RXFE` bit.
One `static UART: Uart` (`:60`) instance; there is no locking around it — see
"Single writer path" below for why that's safe.

## Output: the `emit()` chokepoint

Every print helper (`print`, `print_char`, `print_hex`, `print_dec`,
`print_u64`, `StackWriter::flush`) funnels through `emit(bytes: &[u8])`
(`:73-83`), added in the June 2026 multikernel change. On a `kernel_smp`
build, `emit` first tries `crate::smp::console_emit(bytes)` — if the calling
core is a secondary whose per-core console ring is set, the bytes are
appended to that ring instead of touching the UART (the UART MMIO isn't even
mapped in a secondary's restricted table). The BSP (core 0) drains
secondaries' rings to the real UART on its own schedule. On the BSP, or in
any pre-bringup / non-SMP path, `console_emit` returns `false` (or doesn't
exist) and `emit` falls through to writing the UART directly, with IRQs
disabled (`irq::with_irqs_disabled`, `:78-82`) so a timer preemption can't
interleave two threads' output mid-message.

`print_bytes` (`:90-92`, `#[cfg(kernel_smp)]`) is the raw-byte variant used
by the multikernel console drainer to forward a secondary's ring contents,
which may straddle a UTF-8 boundary and so can't go through `print(&str)`.

## Formatting without heap allocation

`print_hex`/`print_dec`/`print_u64` (`:107-165`) format directly into a
fixed-size stack buffer, no `alloc`. `StackWriter<const N: usize>`
(`:173-209`) implements `core::fmt::Write` over a stack buffer for panic-safe
`write!()`-style formatting; `safe_print!`/`tprint!` (`:219-241`, exported
macros) wrap it — `tprint!` additionally prepends a `[T<secs>.<cs>]` uptime
timestamp read from `crate::timer::uptime_us()`. These exist specifically so
kernel logging can't itself panic from an allocation failure while already
handling a fault.

## Input

`has_char()`/`getchar()` (`:244-251`) are the non-blocking read primitives
(check `has_data()` before calling `getchar`). `getchar_blocking()` and
`read_line()` (`:255-277`, both `#[allow(dead_code)]`) are unused convenience
wrappers kept for callers outside the normal async SSH input path.

## Background

- `docs/archive/MULTIKERNEL.md` (§8.2) — the per-core SPSC console ring
  design this file's `emit()`/`print_bytes` split exists to serve: producer
  is the secondary core, consumer is the BSP's drain loop.
