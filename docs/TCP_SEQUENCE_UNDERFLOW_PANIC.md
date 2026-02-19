# TCP Sequence Underflow Panic

**Date:** February 9, 2026  
**Issue:** Kernel panic during heavy network load (`scratch clone`).

## Symptom

During a `scratch clone` operation, specifically after receiving sideband data, the kernel panics with the following message:

```
!!! PANIC !!!
Location: .../smoltcp-0.12.0/src/wire/tcp.rs:81
Message: attempt to subtract sequence numbers with underflow
```

## Context

*   **Operation:** `scratch clone https://github.com/netoneko/akuma.git`
*   **Userspace State:** `scratch` reported sideband buffers of ~64-65KB just before the crash.
*   **Kernel State:** Using `embassy-net` (which wraps `smoltcp`).

## Initial Analysis

The panic occurs inside `smoltcp`'s TCP wire format handling. Line 81 in `wire/tcp.rs` (v0.12.0) typically involves calculating sequence number distances or window sizes.

An "attempt to subtract sequence numbers with underflow" usually suggests:
1.  **Out-of-order Packet Handling:** A packet was received with a sequence number that `smoltcp` considered "behind" its current window in a way that triggered a raw subtraction instead of a modular distance check.
2.  **State Corruption:** Multi-threaded access to the `!Sync` network stack (protected by `disable_preemption`) might have a race condition that corrupts the internal `TcpSocket` state.
3.  **Large Window/Sideband Interaction:** The `scratch` logs show large sideband buffers. It's possible the volume of data is hitting a corner case in `smoltcp`'s TCP window management.

## Relation to Recent Changes (Strategy A)

We recently increased `NETWORK_THREAD_RATIO` and syscall persistence. While this improves performance, it also changes the timing of network stack polling:
*   Thread 0 polls more frequently.
*   Syscall threads stay in the kernel longer.

It is currently unknown if these timing changes will exacerbate or mitigate this bug. If the bug is a race condition, increased frequency of access might make it more likely to appear.

## Investigation Plan

1.  **Reproduce:** Attempt to trigger the panic consistently with `scratch clone`.
2.  **Version Check:** Verify if `smoltcp` 0.12.0 has known issues with sequence number underflow (check GitHub issues for `m-labs/smoltcp`).
3.  **Trace Logging:** If reproducible, enable packet-level logging in `src/embassy_virtio_driver.rs` or `smoltcp` to see the sequence numbers of the failing packet.
4.  **Strategy B Alignment:** If this is an inherent `embassy-net` wrapping issue, it provides further justification for migrating to Strategy B (Direct `smoltcp` with `Spinlock`).
