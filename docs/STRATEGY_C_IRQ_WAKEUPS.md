# Strategy C: IRQ-Driven Wakeups

**Objective:** Eliminate polling and `yield`-loops by implementing a reactive wakeup system based on hardware interrupts.

## Tasks

1.  **Enhance VirtIO Driver**
    *   **File:** `src/embassy_virtio_driver.rs` or equivalent.
    *   **Action:** Modify the IRQ handler to signal the scheduler when specific packets arrive.
    *   **Goal:** Move away from Thread 0 polling the VirtIO device in a tight loop.

2.  **Socket Wait Queues**
    *   **File:** `src/socket.rs`.
    *   **Action:** Add a `WaitQueue` (or a list of wakers) to each `KernelSocket`.
    *   **Goal:** Allow threads to block on a specific socket and be woken up only when that socket has data.

3.  **Scheduler Integration**
    *   **File:** `src/threading.rs`.
    *   **Action:** Implement `block_on(condition)` where the thread is parked until an IRQ handler calls `wake(thread_id)`.
    *   **Goal:** Ensure 0% CPU usage for threads waiting on network I/O.

4.  **Zero-Copy Path (Future)**
    *   **Action:** Explore mapping VirtIO buffers directly into userspace address space.
    *   **Goal:** Eliminate the kernel-to-userspace copy for high-speed transfers.

5.  **Efficiency Testing**
    *   Monitor CPU usage of the "idle" system during a heavy download.
    *   Compare context switch counts between Strategy A and Strategy C.
