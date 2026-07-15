# log syscalls

Per-process syscall trace ring buffer, surfaced at `/proc/<pid>/syscalls` and
auto-dumped on a WILD-DA crash. Source: `src/syscall/log.rs`. Note: despite
the family label in [`../syscalls.md`](../syscalls.md)'s submodule table
("kernel log (dmesg)"), there is no global dmesg-style buffer here — each
process gets its own small, bounded history.

> **Stability: A (stable, dormant).** Two commits total since the split (a
> clippy fix and the original procfs wiring). The recurring lesson:
> **CLONE_VM sibling threads share the owner PID's log** —
> `read_current_pid()` resolves to the thread-group owner for every sibling,
> so a crashing thread's syscall history is filed under the owner's PID, not
> its own tid.

## Storage

`SYSCALL_LOG`: a `Spinlock<BTreeMap<u32, ProcessSyscallLog>>` keyed by pid
(`log.rs:20`). Each entry is a `VecDeque<SyscallEntry>` (`timestamp_us`, `nr`,
`duration_us`, `result`) capped at `config::PROC_SYSCALL_LOG_MAX_ENTRIES` (64,
oldest dropped on overflow) plus an `exited_at_us` marker used for retention.

- `record()` (`log.rs:23`) — called from `handle_syscall` (`mod.rs:992`)
  after every syscall, but only when both `need_timing` is set and
  `config::PROC_SYSCALL_LOG_ENABLED` is true (forced off on
  `kernel_profile_extreme`, which also skips the per-syscall timing read
  entirely).
- `mark_exited()` (`log.rs:37`) — called from `sys_exit`/`sys_exit_group`
  (`proc.rs:214,300`) to start the retention window instead of deleting the
  entry immediately.
- `get_formatted()` (`log.rs:47`) — lazily evicts entries whose
  `exited_at_us` is older than `PROC_SYSCALL_LOG_RETAIN_MS` (10 s) on every
  call, then renders a fixed-width text table. Consumed by `vfs/proc.rs` for
  `/proc/<pid>/syscalls` and by the crash-dump path in
  `exceptions.rs:2979`, which auto-prints the log on a WILD-DA fault for
  post-crash diagnosis.
- `list_pids_with_logs()` (`log.rs:82`) — backs the `/proc` directory
  listing so a recently-exited pid stays visible until its retention window
  expires.

## Background

- `archive/PROCFS.md` — the full `/proc` filesystem this log feeds,
  including the on-disk table format and retention behaviour.
