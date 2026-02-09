# Strategy B: Smoltcp Migration

**Objective:** Replace the complex, `!Sync` `embassy-net` stack with a direct `smoltcp` implementation protected by kernel-level synchronization.

## Tasks

1.  **Introduce Kernel Network Interface**
    *   **File:** `src/network.rs` or `src/smoltcp_net.rs`.
    *   **Action:** Create a global `Spinlock<smoltcp::iface::Interface>`.
    *   **Goal:** Provide a thread-safe entry point for all network operations.

2.  **Refactor Socket Management**
    *   **File:** `src/socket.rs`.
    *   **Action:** Change `KernelSocket` to store `smoltcp::socket::tcp::Socket` instead of `embassy_net::tcp::TcpSocket`.
    *   **Goal:** Remove the need for `RefCell` and `disable_preemption` inside the socket layer by using the global interface lock.

3.  **Unified Polling Engine**
    *   **File:** `src/async_net.rs` (to be deprecated or refactored).
    *   **Action:** Implement a `poll_iface()` function that handles ARP, TCP retransmits, and interface processing.
    *   **Goal:** Allow both the background idle thread and active syscall threads to drive the network stack.

4.  **Remove Embassy Dependencies**
    *   **Action:** Remove `embassy-net`, `embassy-time`, and `embassy-executor` from the kernel core where possible.
    *   **Goal:** Reduce binary size and simplify the execution model.

5.  **Performance Verification**
    *   Measure the overhead of the `Spinlock` versus the current `disable_preemption` model.
    *   Verify that concurrent socket access (e.g., multiple SSH sessions) remains stable.
