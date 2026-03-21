# Go Runtime — Missing / Incomplete Syscall Support

Tracked gaps and fixes required to run Go binaries on Akuma.

---

## 50. Pipe Read Performance (Quadratic Slowdown) (2026-03-20)

**Status:** Fixed (2026-03-20) in `src/syscall/pipe.rs`
**Component:** `src/syscall/pipe.rs`

### Symptom

Large Go builds would hang or take extremely long (minutes) inside the kernel during `read` syscalls, with logs showing `in_kernel` times exceeding 100 seconds.

### Root cause

The `KernelPipe` implementation used a `Vec<u8>` for its buffer. `pipe_read` performed `pipe.buffer.drain(..n)`, which is an **O(N)** operation because it shifts all remaining elements in the vector. For a process writing megabytes of data, every small read (e.g., 4KB) triggered a massive memory shift, leading to quadratic $O(N^2)$ performance.

### Fix

Replaced `Vec<u8>` with `VecDeque<u8>` in `KernelPipe`. `VecDeque::drain` is efficient (O(1) amortized for front removal), eliminating the memory shifting bottleneck.

This is verified by the `test_pipe_large_transfer` test in `src/sync_tests.rs`, which transfers 1MB of data in 1KB chunks.

## 51. `ChildStdout` streaming hangs — parent busy-looping on non-blocking read (2026-03-20)

**Status:** Fixed (2026-03-20) in `crates/akuma-exec/src/process/mod.rs` and `src/syscall/fs.rs`
**Component:** `crates/akuma-exec` — `ProcessChannel`, `src/syscall/fs.rs` — `sys_read`

### Symptom

Parent processes (like `go build` or the Shell) would hang or lose output when reading from a child's stdout. The kernel log showed frequent `[epoll] pwait` loops with 0 events, and PSTATS indicated excessive time spent in `futex` or busy-looping, indicating the parent was not waiting for the child to produce data.

### Root cause

1.  **Non-blocking Reads**: The `sys_read` implementation for `FileDescriptor::ChildStdout` was non-blocking: it called `ch.read()`, and if no data was immediately available, returned `0` (EOF/empty). The parent process, instead of blocking until data arrived, would immediately loop and try again (busy-looping) or, worse, incorrectly assume the child had finished (premature EOF).
2.  **Lack of Wait Mechanism**: There was no mechanism for a reader thread to register itself with the `ProcessChannel` and be woken up when the child wrote data, making efficient blocking reads impossible.

### Fix

1.  **Blocking `ProcessChannel`**:
    - Added `reader_thread` tracking to `ProcessChannel`. 
    - `ProcessChannel::read()` now registers the caller's thread ID if the buffer is empty.
    - `ProcessChannel::write()` now wakes the `reader_thread` when new data is added, ensuring the reader is resumed only when needed.
2.  **Blocking `sys_read`**:
    - Updated `sys_read` for `ChildStdout` to loop until data arrives or the child process exits.
    - Added `akuma_exec::threading::schedule_blocking(u64::MAX)` to put the thread to sleep, preventing busy-looping while waiting for output.
3.  **Regression Test**: Added `test_child_stdout_blocking_read` in `src/process_tests.rs`, which verifies that parent processes correctly block until the child outputs data.
