# SSHD Userspace Limitations

This document outlines the known technical limitations of the current userspace `sshd` implementation in Akuma OS. These constraints are primarily due to the current state of the userspace runtime environment.

> **§1 and §2 describe the `SSHD_FORK_SESSIONS=0` build only (2026-08-10).**
> The default build now serves each connection from its own forked process, so
> sessions *are* parallel, they *do* use multiple cores, and a blocking syscall
> in one no longer stalls its peers. See
> [`PROCESS_PER_SESSION.md`](PROCESS_PER_SESSION.md). §2's premise — that
> `libakuma` exposes no way to get concurrency — was also wrong: `fork()` was
> always available, it just had no wrapper. There is one now
> (`libakuma::fork`), and `userspace/forkprobe` proves it works from a `no_std`
> binary.
>
> Everything in §1-§2 still applies verbatim if you build with
> `SSHD_FORK_SESSIONS=0`, which memory-constrained images should
> ([`docs/runbooks/build-extreme-size.md`](../../../docs/runbooks/build-extreme-size.md)).
> §3-§6 apply to both builds — and §3 is *more* pressing under the default now
> that each session is a process against a global `MAX_PROCESSES = 64`.

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

## 6. Exit Reporting: What Is and Isn't Covered

Both RFC 4254 §6.10 reports are implemented — `exit-status` for a clean exit,
`exit-signal` for a signal death (see [`EXIT_STATUS_FIX.md`](EXIT_STATUS_FIX.md)).
Remaining gaps:

- **No `WIFSTOPPED` / job control.** A stopped (not terminated) child has no
  encoding anywhere in the stack; `WaitStatus::signaled()` deliberately excludes
  the `0x7F` low byte so that adding one later cannot be misread as a kill.
- **`core_dumped` is always false.** Akuma writes no core files.
- **Signals the shell handles are not signal deaths.** busybox `sh` catches
  SIGTERM/SIGINT/SIGQUIT/SIGSEGV and exits 130, so those are reported as
  `exit-status 130`. This is correct — the shell did exit normally — but it means
  only uncatchable SIGKILL exercises the `exit-signal` path in practice.
- **`libakuma::waitpid` still returns `WEXITSTATUS` only**, reporting 0 for a
  signal death. It was left signature-compatible on purpose (11 call sites in
  `box`/`herd`/`httpd`/`elftest`/`meow`); those callers may want migrating to
  `waitpid_status()`, which would let e.g. `herd` tell a crashed service from a
  clean one.
