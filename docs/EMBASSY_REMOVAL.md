# Embassy Removal

**Status:** Completed on Feb 12, 2026.

**Objective:** Remove the Embassy async runtime and all unused dependencies, replacing the only used feature (async timeouts) with a minimal kernel-owned timer module.

## Background

After the smoltcp migration (Strategy B), Embassy's role had shrunk dramatically:

- **`embassy-executor`**: Polled every main loop tick, but zero tasks were ever spawned on it.
- **`embassy-time`**: Used only for `with_timeout()` in SSH protocol (4 call sites) and `Timer::after()` in the disabled memory monitor.
- **`embassy-time-driver`**: ARM CNTV timer driver backing `embassy-time`.
- **`embassy-sync`**: Not imported anywhere. Dead dependency.

The SSH server already had its own `block_on` loop with a no-op waker. The `TcpStream` async Read/Write impls used `core::future::poll_fn` with smoltcp's waker registration -- pure `core` futures with no Embassy dependency.

## Implementation

### New Module: `src/kernel_timer.rs`

A self-contained ~145-line module that replaces all Embassy functionality:

- **`Duration`** -- minimal duration type (`from_secs`, `from_millis`, `from_micros`).
- **`with_timeout(duration, future)`** -- wraps a future with a deadline. Uses `poll_fn` internally.
- **`Timer::after(duration)`** -- async delay future.
- **Alarm queue** -- 8-slot array of `(deadline, Option<Waker>)`, protected by `critical-section`. Schedules the ARM Virtual Timer (CNTV) to fire at the earliest deadline.
- **`on_timer_interrupt()`** -- called from IRQ 27 handler. Wakes expired wakers outside the critical section to avoid deadlocks.
- **`signal_wake()`** -- ARM SEV instruction (moved from the deleted `executor.rs`).
- **`critical-section` impl** -- DAIF save/restore with nesting counter (moved from `embassy_time_driver.rs`).

### Deleted Files

- `src/executor.rs` -- Embassy executor with unused spawner, IRQ work queue, and WFE/SEV integration.
- `src/embassy_time_driver.rs` -- Embassy time driver. Useful logic (alarm queue, CNTV register access, critical-section impl) was absorbed into `kernel_timer.rs`.

### Modified Files

- **`src/main.rs`**:
    - Replaced `mod embassy_time_driver` / `mod executor` with `mod kernel_timer`.
    - Replaced `embassy_time_driver::init()` with `kernel_timer::init()`.
    - Replaced IRQ 27 handler to call `kernel_timer::on_timer_interrupt()`.
    - Removed `executor::init()`, `executor::process_irq_work()`, and `executor::run_once()` from the main loop.
    - Updated `memory_monitor()` to use `kernel_timer::{Duration, Timer}`.
- **`src/ssh/protocol.rs`**:
    - Replaced `use embassy_time::Duration` with `use crate::kernel_timer::Duration`.
    - Replaced 4 `embassy_time::with_timeout()` calls with `crate::kernel_timer::with_timeout()`.
- **`src/shell_tests.rs`**:
    - Replaced `embassy_time_driver::on_timer_interrupt()` with `kernel_timer::on_timer_interrupt()`.

### Dependencies Removed (9 total)

**Embassy crates:**
- `embassy-executor`
- `embassy-time`
- `embassy-time-driver`
- `embassy-sync`
- `static_cell`

**Other dead dependencies found during audit:**
- `edge-http` -- never imported.
- `nostd-interactive-terminal` -- never imported; `InteractiveRead` trait is kernel-defined.
- `format_no_std` -- only referenced in a comment; `safe_print!` uses `core::fmt::Write`.
- `curve25519-dalek` -- no direct usage; pulled in transitively by `x25519-dalek`.

### Dependencies Kept

- `critical-section` -- still needed for the alarm queue's interior mutability (we provide the `set_impl!`).
- `embedded-io-async` -- defines the async Read/Write traits used by `TcpStream` and SSH protocol.

## Architecture

The async timer path after this change:

```
IRQ 27 (CNTV fires)
  -> kernel_timer::on_timer_interrupt()
  -> checks alarm queue, wakes expired wakers
  -> signal_wake() (ARM SEV)

Async code (SSH protocol, memory monitor):
  -> kernel_timer::with_timeout(duration, future)
  -> on poll: checks deadline, calls schedule_wake()
  -> schedule_wake() sets CNTV_CVAL to earliest alarm
```

No external async runtime is involved. All async code is driven by `core::future`, `core::task`, `poll_fn`, and smoltcp's waker registration.
