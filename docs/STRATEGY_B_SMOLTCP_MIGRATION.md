# Strategy B: Smoltcp Migration (Implemented)

**Status:** Completed on Feb 9, 2026.

**Objective:** Replace the complex, `!Sync` `embassy-net` stack with a direct `smoltcp` implementation protected by kernel-level synchronization.

## Implementation Details

1.  **Kernel Network Interface (`src/smoltcp_net.rs`)**:
    *   Implemented `NetworkState` struct holding `Interface`, `SocketSet`, and devices.
    *   Protected by a global `Spinlock<NetworkState>`.
    *   Implemented `VirtioSmoltcpDevice` wrapper to bridge `virtio-drivers` and `smoltcp::phy::Device`.
    *   Added Loopback support sharing the same `SocketSet`.

2.  **Socket Management (`src/socket.rs`)**:
    *   Refactored `KernelSocket` to use `smoltcp::socket::tcp::SocketHandle`.
    *   Implemented blocking `socket_accept`, `socket_connect`, `socket_send`, `socket_recv` using a `wait_until` helper that polls the global network stack.

3.  **SSH Server (`src/ssh/server.rs`)**:
    *   Refactored to run on a dedicated system thread.
    *   Uses a custom `block_on` helper to run the async SSH protocol handler.

4.  **Main Loop (`src/main.rs`)**:
    *   Simplified to just poll `smoltcp_net` and run background tasks.
    *   Removed `embassy-net` initialization.

5.  **Removed Files**:
    *   `src/async_net.rs`
    *   `src/embassy_net_driver.rs`
    *   `src/embassy_virtio_driver.rs`

## Benefits
*   **Thread Safety:** Any thread can safely access the network stack via the global spinlock.
*   **Preemption:** No need to disable global preemption during network polling.
*   **Performance:** Direct polling from syscalls reduces latency compared to context switching to a dedicated network thread.
