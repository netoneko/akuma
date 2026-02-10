# Smoltcp Migration Post-Mortem (Feb 2026)

## The Core Struggle: The "Unaddressable" Loop
During the migration from `embassy-net` to `smoltcp 0.12.0`, the system entered a failure state where loopback connections (`127.0.0.1`) would consistently timeout or fail with `Unaddressable`.

### 1. The Shared SocketSet Conflict
**Issue**: Initially, I implemented two interfaces (VirtIO and Loopback) sharing a single `SocketSet`.
**Symptom**: Packets destined for `127.0.0.1` were being intercepted by the VirtIO interface's poller (which ran first). Because the VirtIO interface didn't "own" the loopback IP, it dropped the packets, but because it had already consumed them from the `SocketSet`, the actual Loopback interface never saw them.
**Resolution**: Simplified to a **Single Interface** model. The main interface now holds both the external IP and `127.0.0.1`. `smoltcp` handles the internal routing via software loopback when it detects a local destination.

### 2. Ephemeral Port Blindness
**Issue**: The `connect` syscall was passing `0` as the local port.
**Symptom**: `smoltcp` returned `Unaddressable`. Unlike higher-level OS stacks, `smoltcp`'s core `connect` method requires a specific local port and does not perform automatic ephemeral allocation at that layer.
**Resolution**: Implemented a thread-safe **Kernel Ephemeral Port Allocator** (`AtomicU16` in `src/socket.rs`) that assigns ports in the `49152–65535` range before calling into the stack.

### 3. The "Pending" Silence (Waker Misses)
**Issue**: SSH and HTTP services would start but never respond to data.
**Symptom**: The async executor was putting threads to sleep, but they were never waking up.
**Resolution**: Discovered that `TcpStream::read` and `TcpStream::write` were returning `Poll::Pending` without calling `socket.register_recv_waker(cx.waker())`. Without this registration, the network stack has no way to tell the kernel scheduler that data is ready.

### 4. DHCP Deconfiguration Flakiness
**Issue**: QEMU's Slirp DHCP server often deconfigures or "naks" the lease shortly after boot.
**Symptom**: The kernel would clear its IP address, breaking SSH forwarding.
**Resolution**: Implemented a **Static IP Fallback** (`10.0.2.15`). If DHCP deconfigures, the kernel automatically restores the fallback instead of leaving the interface unconfigured.

## Summary of Architectural Changes
- **Stack Version**: Upgraded to `smoltcp 0.12.0` (Core Net types).
- **Architecture**: Single-interface, multi-IP, software loopback.
- **Concurrency**: Spinlock-protected global `NetworkState`.
- **Reliability**: Boot-time self-tests with detailed panic diagnostics.
