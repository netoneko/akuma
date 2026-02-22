# SSHD Userspace Limitations

This document outlines the known technical limitations of the current userspace `sshd` implementation in Akuma OS. These constraints are primarily due to the current state of the userspace runtime environment.

## 1. Single-Session Restriction (Serial Processing)
The most significant limitation is that the server can only handle **one active connection at a time**.

- **Cause**: The `main` loop calls `handle_connection`, which is a blocking call. The server will not return to the `accept()` state until the current SSH session is terminated (e.g., the user logs out or the connection drops).
- **Impact**: New connection attempts will time out or hang if another user is already connected.

## 2. Lack of Userspace Threading
Akuma OS currently does not provide a mechanism for userspace processes to manage multiple threads of execution.

- **Missing Infrastructure**: There is no `sys_thread_create` or `fork()` syscall available to `libakuma`.
- **Future Requirement**: To support concurrent connections, the kernel must support thread spawning within a process's address space, and the `sshd` server must be updated to an async-spawn or thread-per-connection model.

## 3. Kernel Socket Limits
All networking in userspace eventually relies on the kernel's network stack.

- **Global Limit**: The kernel is configured with `MAX_SOCKETS: 128`. This limit is shared across all processes (including `httpd`, `herd`, `sshd`, and raw syscalls).
- **Resource Exhaustion**: If too many sockets are left in a `TIME_WAIT` state or leaked by processes, `sshd` may fail to bind or accept new connections even if no sessions are active.

## 4. Memory Considerations
SSH is a cryptographically heavy protocol, making it resource-intensive for a `no_std` userspace application.

- **Buffer Allocations**: Each session maintains several `Vec<u8>` buffers for incoming packets, decrypted payloads, and channel data.
- **Crypto Overhead**: `aes-ctr`, `hmac-sha256`, and `ed25519` operations require temporary heap allocations that may be significant in memory-constrained environments.
- **Stack Usage**: While the default stack is 128KB, deep async call chains or complex shell commands could potentially approach this limit.

## 5. Shell Integration Bottlenecks
- **I/O Bridging**: In the current "Bridge" mode (when using an external shell like `paws`), I/O is forwarded via synchronous syscalls. This may lead to high latency or dropped characters if the scheduler doesn't context-switch between the bridge and the shell process frequently enough.
- **Bidirectional I/O**: Full bidirectional interaction (writing to a child process's stdin from `sshd`) is still experimental and may not behave exactly like a real PTY.
