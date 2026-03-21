# Fix Go build hang and procfs bugs

## Status

- **Fix 1** (exit_group crash) — **DONE**: `kill_thread_group` marks siblings Zombie instead of unregistering them; they're removed from `PROCESS_TABLE` only when the last thread slot is recycled.
- **Fix 2** (epoll pipe wakeup) — **DONE**: `pipe_write` drains the `pollers` BTreeSet and wakes each waiting thread via `wake_thread`.
- **Fix 3** (fd table not closed on exit_group) — **DONE**: `sys_exit_group` now calls `proc.fds.close_all()` before `kill_thread_group`, ensuring pipe write-ends are decremented immediately and epoll pollers (e.g. Go's parent waiting for compile stdout EOF) are woken synchronously.
- **Fix 4** (procfs fd listing) — **DONE**: `/proc/<pid>/fd/` now enumerates all open fds from the BTreeMap instead of hardcoding only "0" and "1".
- **Fix 5** (procfs syscalls spurious entry) — **DONE**: The "syscalls" entry in `/proc/<pid>/` is only added when log data actually exists (`get_formatted(pid).is_some()`).

---

## Root cause of Go hang (fixed)

`sys_exit_group` marked the process Zombie and called `kill_thread_group` to kill sibling goroutine threads. Both `kill_thread_group` and `return_to_kernel` call `cleanup_process_fds()`, which only calls `close_all()` when `Arc::strong_count(&proc.fds) == 1`. With N live sibling threads in `PROCESS_TABLE` each holding an Arc reference, this check always failed — the shared `SharedFdTable` (and its `PipeWrite` fds) was never explicitly closed. Go's parent epoll waited forever for pipe EOF.

**Fix**: Call `proc.fds.close_all()` explicitly in `sys_exit_group` before `kill_thread_group`. `close_all()` is idempotent — it drains and clears the table atomically, so `cleanup_process_fds()` later finds an empty table and skips any double-close. `pipe_close_write` is called for each `PipeWrite` fd, which decrements `write_count` and wakes all epoll pollers.

---

## Remaining work

- **SIGPIPE delivery**: Kernel does not auto-deliver `SIGPIPE` on write-to-dead-pipe. Linux does. The Go runtime compensates with manual `tgkill(SIGPIPE)`, which is fragile. This is secondary now that EOF is delivered correctly, but should be implemented for full compatibility.

---

## Verification

1. `cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)`
2. `cargo run --release` — boot in QEMU
3. `go build -v -o /tmp/out .` — compile processes exit cleanly, parent receives EOF, build completes
4. `find /proc/` — no "failed to stat /proc/<pid>/syscalls" errors
5. `ls -la /proc/<go-pid>/fd/` — shows all fds, not just 0 and 1
