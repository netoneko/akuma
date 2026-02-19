# Fix: Terminal Input Polling Overflow

## Problem
The `termtest` userspace program (and any other program using blocking input) exited immediately instead of waiting for user input when `poll_input_event` was called with an infinite timeout (`u64::MAX`).

## Root Cause Analysis
The issue was caused by multiple integer overflows during time conversion and deadline calculation:

1.  **Userspace Overflow:** In `libakuma`, `poll_input_event` converted the `timeout_ms` (milliseconds) to microseconds by multiplying by `1000`. For `u64::MAX`, this multiplication overflowed, resulting in a very small value being passed to the kernel syscall.
2.  **Kernel Overflow:** In `sys_poll_input_event`, the kernel added this (already small) timeout to `uptime_us()` to calculate a deadline. If the addition overflowed or resulted in a value smaller than the current time, the polling loop would terminate immediately.

## Resolution

### Userspace (libakuma)
- **`poll_input_event`**: Added explicit handling for `core::u64::MAX`. If the timeout is infinite, it passes `u64::MAX` directly to the kernel as microseconds. Otherwise, it uses `saturating_mul(1000)`.
- **`sleep_ms`**: Updated to use `saturating_mul(1_000_000)` when converting milliseconds to nanoseconds for `NANOSLEEP`.

### Kernel (src/syscall.rs)
- **`sys_poll_input_event`**: Changed deadline calculation to use `saturating_add`.
- **`sys_nanosleep`**: Updated to use `saturating_mul` and `saturating_add` for all time-related calculations.

## Impact
Blocking terminal input now correctly waits for events when an infinite or very large timeout is specified. General time-related syscalls are now more robust against overflow-induced logic errors.
