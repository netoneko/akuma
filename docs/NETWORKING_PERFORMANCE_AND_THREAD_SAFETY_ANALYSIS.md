# Networking Performance and Thread Safety Analysis

**Date:** February 9, 2026  
**Issue:** Slow networking during repository clones (`scratch clone`) and potential thread-safety issues with `embassy-net`.

## Current State Analysis

### 1. Bottlenecks in TLS Layer
Although `docs/TLS_DOWNLOAD_PERFORMANCE.md` claimed to reduce sleeps from 10ms to 1ms, `userspace/libakuma-tls/src/transport.rs` still contains `libakuma::sleep_ms(10)`. 

For a 4MB repository clone (approx. 1000 TLS records), hitting `WouldBlock` once per record results in **10 seconds of idle time**. In practice, multiple `WouldBlock` hits occur per record, leading to the observed extreme slowness.

### 2. Thread 0 Scheduling Throttling
The kernel uses `NETWORK_THREAD_RATIO = 4` to balance CPU between the network runner (Thread 0) and userspace threads. This limits Thread 0 to ~25% CPU share. When a userspace process like `scratch` is doing heavy CPU work (TLS decryption), Thread 0 may not run frequently enough to drain the VirtIO RX queue, leading to packet loss and TCP window shrinkage.

### 3. Concurrency Model
Akuma currently protects `embassy-net` (which is `!Sync` and uses `RefCell`) by disabling preemption and IRQs on a single CPU. While safe for single-core execution, it creates a serialized bottleneck where only one thread (either Thread 0 or a syscall thread) can interact with the network stack at a time.

## Evaluation of `smoltcp` Alternative

The user suggested replacing `embassy-net` with `smoltcp` for better thread safety.

| Feature | `embassy-net` (Current) | `smoltcp` (Proposed) |
|---------|-------------------------|----------------------|
| **Thread Safety** | Relies on external serialization (preemption/IRQ disable) | `!Sync`, but easier to wrap in a global `Spinlock<Interface>` |
| **Async Support** | Native (requires an executor/main loop) | Purely synchronous/polled (matches Akuma's syscall model better) |
| **Complexity** | High (extra async layer, RefCells) | Lower (direct access to state) |
| **Poll Model** | Requires dedicated runner (Thread 0) | Can be polled on-demand from any syscall or timer |

**Conclusion:** Replacing the `embassy-net` async wrapper with direct `smoltcp` usage protected by a kernel `Spinlock` would simplify the architecture and likely improve performance by allowing any thread to drive the stack when needed.

## Proposed Mitigation Strategies

### Strategy A: Immediate Performance Tuning (Low Effort)

1.  **Fix `libakuma-tls` Sleep**: Reduce `sleep_ms(10)` to `sleep_ms(1)` in `userspace/libakuma-tls/src/transport.rs`.
2.  **Adjust `NETWORK_THREAD_RATIO`**: Reduce from 4 to 2 (or 1) during heavy network operations to give Thread 0 more priority.
3.  **Increase Syscall Retries**: Increase `EAGAIN_ITERATIONS` in `sys_recvfrom` from 50 to 500 to reduce the number of userspace context switches.

### Strategy B: Refactor to Direct `smoltcp` (Medium Effort)

1.  **Decouple from `embassy-net`**: Replace `embassy-net` `Stack` with a global `Spinlock<smoltcp::iface::Interface>`.
2.  **Unified Polling**: Implement a `poll_network()` function that can be called by both Thread 0 (background maintenance) and socket syscalls (on-demand data retrieval).
3.  **Eliminate `RefCell`**: Using a `Spinlock` instead of `disable_preemption` + `RefCell` provides clearer thread-safety semantics and prepares for potential SMP support.

### Strategy C: Improved Thread Coordination (High Effort)

1.  **Direct Wakeups**: Instead of Thread 0 polling everything, have the VirtIO IRQ handler identify which socket has data and wake the specific waiting thread.
2.  **Shared Memory Ring Buffers**: Implement a lock-free RX/TX ring buffer between the driver and userspace to minimize kernel-to-userspace transitions.

## Implementation Plan (Recommended)

1.  **Step 1 (Immediate)**: Update `libakuma-tls` and `scratch` to use 1ms sleeps and verify immediate gains.
2.  **Step 2 (Architectural)**: Propose a design for a `smoltcp` wrapper that eliminates the need for the async `embassy-net` layer in the kernel.
3.  **Step 3 (Verification)**: Run `scratch clone` and measure kbps before and after changes.
