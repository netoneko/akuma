# Strategy A: Stability and Performance Tuning

**Objective:** Minimize latency and maximize throughput by fixing architectural starvation and deadlocks caused by preemption-disabled I/O.

## Tasks

1.  **Prevent Priority Inversion Deadlocks**
    *   **Files:** `src/vfs/mod.rs`, `src/block.rs`
    *   **Change:** Disable preemption while holding `MOUNT_TABLE` and `BLOCK_DEVICE` spinlocks.
    *   **Reason:** Previously, a preemptible thread (like `scratch`) could be switched out while holding a VFS lock. If a preemption-disabled thread (like Thread 0 or an SSH session) then tried to acquire the same lock, it would spin forever (Deadlock).

2.  **Make I/O Preemption-Aware**
    *   **File:** `src/async_fs.rs`
    *   **Change:** Temporarily enable preemption during synchronous FS calls inside async functions.
    *   **Reason:** Async polls in Akuma run with preemption disabled. If a poll calls a slow synchronous I/O function (like reading `authorized_keys`), the entire system hangs. By yielding and enabling preemption during the sync call, we allow the network thread to run during disk I/O.

3.  **Restore Stable Scheduling Constants**
    *   **Files:** `src/config.rs`, `src/syscall.rs`
    *   **Change:** Revert `NETWORK_THREAD_RATIO` to 4 and `EAGAIN_ITERATIONS` to 50.
    *   **Reason:** Aggressive tuning (ratio=2, iterations=500) caused too much starvation and triggered watchdog warnings. The stability fixes in tasks 1 & 2 are more effective than aggressive prioritization.

4.  **Audit Userspace Sleeps**
    *   **Files:** `userspace/libakuma-tls/src/transport.rs`, etc.
    *   **Change:** Ensure all `WouldBlock` handlers use 1ms sleeps (not 10ms).
    *   **Status:** Done. This provides immediate gains for TLS/HTTPS without kernel instability.

## Verification
*   Run `scratch clone` and verify `kbps` is significantly higher than 3 kbps.
*   Log in via SSH during a download and verify no watchdog triggers or hangs.
