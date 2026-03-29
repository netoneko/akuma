# Epoll Performance Optimization

This document summarizes the optimizations implemented to improve the responsiveness and scalability of the `epoll` subsystem in Akuma OS.

## 1. Elimination of Loop Allocations
The previous implementation of `sys_epoll_pwait` performed a `Vec` allocation (`interest_snapshot`) in every iteration of its polling loop. This was extremely expensive for high-frequency polling or large interest lists.

**Change:** 
- Removed the `interest_snapshot` allocation.
- The loop now locks the `EPOLL_TABLE` and iterates directly over the `interest_list` using an internal iterator.
- Reduced memory pressure and allocator overhead significantly during high-load I/O.

## 2. Event-Driven Wakeups (Reactive Polling)
Previously, `epoll_pwait` relied on a fixed 10ms (`BLOCKING_POLL_INTERVAL_US`) sleep when no events were ready. This introduced up to 10ms of latency for every network or pipe event.

**Change:**
- Implemented a `Waker`-based registration system.
- `epoll_check_fd_readiness` now accepts an optional `Waker` (from the current thread).
- When `epoll_pwait` blocks, it registers its thread waker with all FDs in the interest list.
- Resources (Sockets, Pipes, EventFds, etc.) now trigger these wakers IMMEDIATELY when an event occurs (data arrival, space available, or EOF).

## 3. Multi-Poller Support
Many resources previously only supported a single `reader_thread`. This meant that if multiple threads or an `epoll` instance were waiting on the same FD, only one would be woken.

**Change:**
- Replaced `reader_thread: Option<usize>` with `pollers: BTreeSet<usize>` in:
    - `KernelPipe` (Pipes)
    - `KernelEventFd` (EventFds)
    - `ProcessChannel` (Stdin, ChildStdout, PidFd)
- All registered pollers are now woken upon data writes or state changes.

## 4. Socket Wakeup Infrastructure
Integrated `smoltcp` wakers with the kernel socket table.

**Change:**
- Added `wakers: Spinlock<Vec<Waker>>` to `KernelSocket`.
- `smoltcp_net::poll()` now detects `SocketStateChanged` and triggers `wake_all()` on the corresponding sockets.
- `socket_send` and `socket_recv` trigger `wake_all()` when they make progress, notifying other threads waiting for buffer space or data.

## 5. Unified Readiness Logic
Streamlined the kernel readiness checking logic across all polling syscalls.

**Change:**
- Refactored `ppoll` and `pselect6` to use the unified `epoll_check_fd_readiness` helper.
- This ensures consistent behavior and allows all polling syscalls to benefit from future resource-specific readiness improvements.

## Result
- **Latency:** Average latency for network/pipe events reduced from ~5ms (average of 10ms poll) to sub-millisecond (immediate SGI-driven wakeup).
- **CPU Usage:** Significant reduction in idle CPU usage for I/O-bound applications as they no longer need to "spin" every 10ms if no data is present.
- **Scalability:** Better handling of multiple concurrent readers/pollers on shared file descriptors.

## Bug Report: Lock Inversion Deadlock (Fixed)
During implementation, a deadlock was discovered during parallel process execution.

**Symptoms:**
- System hangs during `spawn_process_with_channel` while other threads are calling `epoll_pwait`.

**Root Cause:**
- `sys_epoll_pwait` held the global `EPOLL_TABLE` lock while calling `epoll_check_fd_readiness`.
- `epoll_check_fd_readiness` (for `Stdin`, etc.) calls `current_process()`, which locks the `PROCESS_TABLE`.
- Meanwhile, `spawn_process` locks `PROCESS_TABLE` to register the new process, and then may indirectly trigger a wakeup that tries to acquire `EPOLL_TABLE` (e.g. if it was already in an interest list).
- This created a classic AB-BA lock inversion.

**Fix:**
- Refactored `sys_epoll_pwait` to **snapshot** the interest list into a stack-allocated array (up to 128 FDs) before performing readiness checks.
- This breaks the lock chain by releasing `EPOLL_TABLE` before acquiring `PROCESS_TABLE`.
- The stack-allocated snapshot avoids heap allocations for the common case, preserving the performance gains of the optimization.
