# Strategy A: Immediate Performance Tuning

**Objective:** Minimize latency and maximize throughput using existing infrastructure by tuning constants and reducing artificial delays.

## Tasks

1.  **Reduce Syscall Polling Overhead**
    *   **File:** `src/syscall.rs`
    *   **Change:** Increase `EAGAIN_ITERATIONS` from 50 to 500 in `sys_recvfrom`.
    *   **Reason:** Currently, the kernel returns `EAGAIN` to userspace very quickly. If data isn't ready, userspace sleeps for 1ms. By staying in the kernel longer (with `yield` between attempts), we allow the network thread more opportunities to process packets without triggering a full userspace context switch and 1ms sleep.

2.  **Increase Network Thread Priority**
    *   **File:** `src/config.rs`
    *   **Change:** Decrease `NETWORK_THREAD_RATIO` from 4 to 2.
    *   **Reason:** This doubles the frequency at which Thread 0 (network runner) is boosted by the scheduler. This ensures that the VirtIO RX queue is drained faster, preventing packet drops and maintaining a larger TCP window.

3.  **Audit Userspace Sleeps**
    *   **Files:** `userspace/libakuma-tls/src/transport.rs`, `userspace/scratch/src/http.rs`, `userspace/libakuma-tls/src/http.rs`.
    *   **Change:** Ensure all `WouldBlock` handlers use 1ms sleeps (not 10ms).
    *   **Status:** `libakuma-tls/src/transport.rs` was updated to 1ms. `scratch/src/http.rs` is already at 1ms. Need to check `libakuma-tls/src/http.rs`.

4.  **Verification**
    *   Run `scratch clone https://github.com/netoneko/akuma.git`.
    *   Monitor `kbps` output.
    *   Check SSH responsiveness during the clone.
