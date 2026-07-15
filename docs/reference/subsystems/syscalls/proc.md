# proc syscalls

fork / clone / vfork / execve / wait / exit. Source: `src/syscall/proc.rs`.
For signal delivery see [`signal.md`](signal.md); for CoW mechanics see
[`../memory.md`](../memory.md).

> **Stability: A (stable).** The Go forktest + rustc bring-up cohort
> (Mar–May 2026) is resolved and dormant. The recurring lesson: **a Linux
> process lifecycle has exactly one reaper** — `exit()`/`kill()` make zombies,
> **only** `wait4`/`waitid`/`waitpid` reap them, and CLONE_THREAD siblings are
> auto-reaped (never zombie).

## clone / fork / vfork

`sys_clone` (`src/syscall/proc.rs`) routes on the flags:

| Flags | Path | Behaviour |
|---|---|---|
| `CLONE_VFORK \| CLONE_VM` | `vfork_process` (`VFORK_FASTPATH_ENABLED`) | child shares parent page tables (`new_shared`), no CoW copy, no parent TLB flush; parent blocks on `VFORK_WAITERS` until child `execve`/`_exit` |
| `CLONE_VM \| CLONE_THREAD` | `clone_thread` | new thread, shared fd table + address space, same `tgid` |
| `SIGCHLD` (low byte 0x11) / bare fork | `fork_process` (`COW_FORK_ENABLED`) | full CoW fork, new `tgid`, deep-copied fd table (pipe refs bumped, EpollFd stripped) |
| anything else | `ENOSYS` | garbage-flag guard below |

**Garbage-flag guard:** Go's `rawVforkSyscall` leaks register state into x0.
The guard rejects `flags >> 32 != 0` (catches `-38`/`-11`/`-22` leaked as
flags) with `ENOSYS` before any routing.

`fork_process` shares the **whole thread group's** address space (code+brk,
stack, the 2 MB interp window at `0x3000_0000`, every sibling's
`mmap_regions`, tgid-keyed lazy regions). **PROCESS_INFO_ADDR (`0x1000`) must
be re-mapped to the child's own frame after `cow_share_range`** — Go ARM64
binaries have `code_start = 0x1000`, so the parent's PTE is otherwise shared
in.

For the CoW share/demote/fault mechanics and invariants, see
[`../memory.md`](../memory.md) "CoW fork" — do not duplicate here.

## execve

`do_execve` → `Process::replace_image[_from_path]`
(`crates/akuma-exec/src/process/image.rs`): **true in-place image replacement**
(preserves PID + open fds; strips O_CLOEXEC fds). Not spawn-as-exec. Calls
`vfork_complete(child_pid)` to wake a blocked parent.

Loader pick (see [`../../abi/linux-compat.md`](../../abi/linux-compat.md)
"ELF loading"): `read_file()` first; on `FsError` (binary > 16 MB), fall back
to `load_elf_from_path` (page-at-a-time via `vfs::read_at()`).

## exit / exit_group

`sys_exit` (93) / `sys_exit_group` (94) (`src/syscall/proc.rs`):

1. `proc.fds.close_all()` **before** terminating the thread (else
   `SharedFdTable::drop` runs in scheduler context and deadlocks).
2. Mark `proc.exited = true`, set exit code.
3. `notify_child_channel_exited(pid, code)` (+ the tgid leader's channel if
   `tgid != pid`, so `wait4(tgid)` unblocks regardless of which thread exited).
4. **Do NOT `unregister_process`** — the process stays as a zombie for
   `wait4` to reap (Linux lifecycle).
5. Terminate the calling thread (`mark_thread_terminated` + yield loop).

`return_to_kernel` (crash path) does **not** group-kill on goroutine crashes
(`exit_code < 0 && tgid != pid`) — a goroutine crash only affects that one
goroutine; the leader and siblings keep running. Group-kill from a crash was
removed because it raced the leader still running on its page tables.

## wait4 / waitid / waitpid

**Blocking, poller-based** — never `yield_now()` busy-spin (the busy-spin
version starved the 32-slot thread pool under Go's 50-worker goroutine pools).

- `pid > 0` / `P_PID` / `P_PIDFD`: register the waiter as a poller on the
  specific child's `ChildChannel` **before** the `has_exited` double-check
  (closes the missed-wakeup race), then `schedule_blocking(u64::MAX)`.
- `pid == -1` / `P_ALL`: `add_poller_to_all_children(current_pid)` — any
  child's `set_exited()` wakes the parent immediately (no 10 ms poll).
- `EINTR` on `is_current_interrupted()`.

`wait4`/`waitid`/`waitpid` **reap** the zombie: `clear_lazy_regions(pid) +
unregister_process(pid)` (the only paths that remove a zombie from the
table). Linux wait-status encoding (`encode_wait_status`): normal exit
`(code & 0xFF) << 8`; signal death `(-code) & 0x7F`.

## kill / tkill

`sys_kill` (`KILL` 302, Linux 129):

- **SIGKILL (9)** bypasses handlers entirely → `kill_thread_group(tgid)` +
  `kill_process_with_signal(pid, 9)` (hard-kill). SIGKILL/SIGSTOP bypass the
  signal mask in `take_pending_signal`.
- Other signals: `pend_signal_for_thread` on the main thread **and** all
  same-`tgid` siblings (each `pend` calls `wake()` internally). **Set all
  `interrupt_thread` flags first, then pend** — the reverse order races
  (wake before flag → thread re-blocks).

## Background

- `archive/ON_DEMAND_ELF_LOADER.md`, `archive/PROPER_EXECVE_PLAN.md`.
- `archive/GO_FORK_EXEC_FIXES.md` — the full Go forktest lifecycle (37 bugs).
- `archive/FORK_MMAP_AND_WAIT_STATUS_FIX.md` — mmap copy on fork +
  wait-status encoding.
- `archive/COW_OPTIMIZATIONS.md` — vfork fast path + frame-teardown O(n²) fix.
- `archive/SIGNAL_DELIVERY.md` — wait/signal interaction, tgid sibling wake.
