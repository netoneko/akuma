# SSHD Userspace Limitations

This document outlines the known technical limitations of the current userspace `sshd` implementation in Akuma OS. These constraints are primarily due to the current state of the userspace runtime environment.

## 1. Single-Threaded Concurrency (Cooperative, Not Parallel)
Concurrent sessions **do** work — this section used to say they didn't, which has
been wrong since the cooperative multiplexer landed (see
[`FLOW.md`](FLOW.md#after-one-future-per-connection-polled-cooperatively)).
What remains is that the concurrency is cooperative, not parallel.

- **Mechanism**: Each connection is one future in a `Vec`, polled round-robin by
  the loop in `main()`. The listener and every accepted socket are non-blocking,
  and `SshStream`'s `Read`/`Write` return `Poll::Pending` on `WouldBlock`, so an
  idle session yields instead of stalling the others.
- **Constraint**: It is all one OS thread. Any session that performs a genuinely
  *blocking* syscall stalls **every** other session for its duration — this is
  exactly the `sleep_ms`-in-a-loop bug documented in `FLOW.md`. Use
  `crate::yield_now().await`, never `sleep_ms`, inside anything a session future
  can reach.
- **Impact**: Sessions doing heavy CPU or blocking I/O add latency to their
  peers. Real parallelism would need userspace threading (§2).

## 2. No Userspace Threading
`sshd` cannot spread sessions across cores or threads within its own process.

- **Missing Infrastructure**: `libakuma` exposes no `sys_thread_create`; `spawn`
  creates a whole separate process, not a thread sharing the address space.
- **Consequence**: The cooperative model in §1 is the only option available. A
  thread-per-connection or work-stealing design would need kernel support first.

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

## 6. Signal-Killed Commands Report Exit Code 0

A remote command that **exits normally** reports its real status (RFC 4254 §6.10
`exit-status`; see `EXIT_STATUS_FIX.md`). One killed by a **signal** does not.

- **Cause**: `libakuma::waitpid` decodes only `WEXITSTATUS`
  (`(status >> 8) & 0xFF`) and discards the raw wait status, so `sshd` cannot
  tell a signal death from a clean exit. A signal status has nothing in the
  high byte, so it decodes as 0.
- **Impact**: `ssh box 'kill -9 $$'` reports success. Real OpenSSH would send an
  `exit-signal` request naming the signal, and its client would exit 255.
- **Fix Requirement**: `waitpid` must surface the raw status (or a
  `WIFSIGNALED`/`WTERMSIG` pair) before `sshd` can emit `exit-signal`. That is a
  `libakuma` change, not an `sshd` one.
