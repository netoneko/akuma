# Smoltcp Migration & Network Architecture (Feb 2026)

## Overview
Successfully migrated the Akuma network stack from `embassy-net` to a direct `smoltcp 0.12.0` integration. This change was motivated by severe I/O starvation and deadlock issues in the previous architecture.

## Work Completed
1.  **Direct Smoltcp Integration**: Removed `embassy-net` and `embassy-net-driver`. Replaced with a kernel-native `NetworkState` protected by a `Spinlock` in `src/smoltcp_net.rs`.
2.  **Thread-Safe Driver**: Implemented `VirtioSmoltcpDevice` which wraps `virtio-drivers` with a `smoltcp::phy::Device` interface.
3.  **Blocking Socket API**: Refactored `src/socket.rs` to provide blocking `connect`, `accept`, `send`, and `recv` syscalls. These use a `wait_until` mechanism that yields the thread to the scheduler while polling the network stack.
4.  **DHCP Implementation**: Restored DHCP support using `smoltcp::socket::dhcpv4`. The stack now automatically configures its IP and default route.
5.  **Scaling**: Increased `MAX_SOCKETS` to 128 to support concurrent services like `httpd` and `ssh`.
6.  **Compatibility**: Updated all IP and address handling to comply with `smoltcp 0.12.0` (using `core::net` style types).

## Challenges Faced
1.  **Priority Inversion**: In the old `embassy-net` stack, non-preemptible network polling would block while waiting for VFS locks held by preemptible threads that were starved by the poller itself.
2.  **Smoltcp 0.12 API Changes**: Significant breaking changes in `smoltcp 0.12` required rewriting address construction (`Ipv4Address::new` vs array literals) and result handling (`PollResult` enum).
3.  **Waker Registration**: In `embedded-io-async` implementations (used by SSH), manual waker registration via `cx.waker()` was required to ensure async tasks resume correctly after network progress.
4.  **Packet Processing Loops**: Ensuring that the `poll()` loop correctly identifies "progress" (using an atomic `POLL_COUNT`) to prevent redundant `yield_now()` calls and high CPU usage.

## Current Status
- Kernel builds in `--release`.
- Network initializes and starts DHCP.
- `httpd` and `ssh` servers are started.
- Loopback networking works via `LoopbackAwareDevice` (intercepts 127.x.x.x at the device layer).
- External connectivity (SSH, HTTP) restored after fixing VirtIO receive bugs. See `docs/VIRTIO_RECEIVE_FIX.md`.
