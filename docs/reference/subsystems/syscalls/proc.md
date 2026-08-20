# proc syscalls

fork / clone / vfork / execve / wait / exit, plus the credential queries.
Source: `src/syscall/proc.rs`. For signal delivery see
[`signal.md`](signal.md); for CoW mechanics see [`../memory.md`](../memory.md).

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

### TID vs PID — the two id namespaces

A `CLONE_THREAD` child gets **two** unrelated ids, from two different counters:

| id | Where it comes from | What it indexes |
|---|---|---|
| **TID** (kernel thread slot) | `threading::spawn_user_thread_initializing` | every per-thread array: pending signals, signal masks, sigaltstacks, wakers, saved contexts |
| **PID** | `allocate_pid()`, stored as `Process::pid` | the process table, fd tables, CHILD_CHANNELS, `wait4` |

`Process::thread_id` maps PID → TID; `THREAD_PID_MAP` maps TID → PID.

**Everything userspace sees as a "tid" must be the TID.** Three syscalls
publish one, and all three must agree:

- `gettid()` → `threading::current_thread_id()`
- `clone(CLONE_PARENT_SETTID)` → the word written at `parent_tid`, **and**
  clone's return value (`clone_thread`)
- `set_tid_address()` → its return value (`sys_set_tid_address`)

### The three tid flags are not interchangeable

They differ in *which* pointer, *what value*, and *when* — and the "when" is
what bites:

| flag | pointer | value | when |
|---|---|---|---|
| `CLONE_PARENT_SETTID` (`0x0010_0000`) | `parent_tid` | child tid | at clone, in the parent |
| `CLONE_CHILD_SETTID` (`0x0100_0000`) | `child_tid` | child tid | when the child first runs, **in the child's context** — so a parent reading it right after `clone` returns sees the old value, on Linux too |
| `CLONE_CHILD_CLEARTID` (`0x0020_0000`) | `child_tid` | **zero** | at child **exit**, followed by a futex wake |

`CLEARTID` says nothing about clone time. Until 2026-08-06 `clone_thread`
wrote `child_tid` unconditionally, i.e. it treated `CLEARTID` as if it also
implied `CHILD_SETTID`. musl's `pthread_create` passes `CLEARTID` *without*
`CHILD_SETTID` and the pointer it passes is `&__thread_list_lock` — a global
mutex word — so every thread spawn stamped a live tid into musl's thread-list
lock, and `__tl_lock`'s `if (val == tid) { tl_lock_count++; return; }`
recursion fast path then handed the lock to the newborn child. It unlinked
itself from the thread list with no lock held and died writing to `0x8`. Full
diagnosis in
[`../../../runbooks/debug-thread-spawn-segv.md`](../../../runbooks/debug-thread-spawn-segv.md)
§2e; regression probe `userspace/forktest/c_stress/tidflags.c`.

The general lesson, and the reason this sits in a stability-A doc: **writing an
output pointer the caller did not ask for is a memory-corruption bug**, not a
harmless extra. Userspace is entitled to keep something else in that word.

`tkill`/`tgkill` take that value straight to `pend_signal_for_thread` and
`thread_signal_mask_of`, which are slot-indexed. Publishing a PID instead
therefore aims every self-signal at whatever unrelated thread happens to sit
in that slot — `clone_thread` and `set_tid_address` both did, which is why
`abort()` never worked on a spawned thread (see
[`signal.md`](signal.md) "Default action for pended signals" and
`archive/SELFHOST_DEVBOX_SMOLTCP.md` "SIGABRT delivery").

**Known residual divergence:** `getpid()` returns `Process::pid`, so each
thread in a group sees a *different* value; Linux returns the shared `tgid`.
`sys_tgkill`'s `proc.tgid != tgid` check consequently rejects a caller that
passes `getpid()` from a non-leader thread. Not yet fixed — `getpid` is load
bearing for `wait4`/CHILD_CHANNELS keying, so the change needs its own pass.

## execve

`do_execve` → `Process::replace_image[_from_path]`
(`crates/akuma-exec/src/process/image.rs`): **true in-place image replacement**
(preserves PID + open fds; strips O_CLOEXEC fds). Not spawn-as-exec. Calls
`vfork_complete(child_pid)` to wake a blocked parent.

Loader pick (see [`../../abi/linux-compat.md`](../../abi/linux-compat.md)
"ELF loading"): `read_file()` first; on `FsError` (binary > 16 MB), fall back
to `load_elf_from_path` (page-at-a-time via `vfs::read_at()`).

### `#!` scripts

Both `execve` **and** `spawn` resolve them, through one shared parser
(`akuma_exec::process::{parse_shebang, shebang_hop}`): `exec_shebang`
(`src/syscall/proc.rs`) for the exec path, `resolve_shebang_chain`
(`crates/akuma-exec/src/process/spawn.rs`) for the SPAWN abi. Up to 4 hops,
matching Linux's `BINPRM_MAX_RECURSION`; at most one argument after the
interpreter, **not** split on whitespace (`#!/usr/bin/env -S a b` passes
`-S a b` as one argv entry).

Two rules that are easy to get wrong and were both wrong here until 2026-08-16:

- **`argv[0]` is the interpreter as *written* in the `#!` line, never its
  symlink-resolved target.** The resolved path is for loading the image only.
  `exec_shebang` used to shadow one with the other, which is fatal rather than
  cosmetic on a busybox system: busybox dispatches entirely on `argv[0]`, so
  `#!/bin/sh` ran `/bin/busybox` with `argv[0]="/bin/busybox"` and busybox never
  knew it was meant to be a shell.
- **Spawn resolves the chain inside the namespace override**, because a
  container's `/bin/sh` exists only in its own mount table; reading the shebang
  from box 0's view finds the wrong interpreter or none. `spawn` had no shebang
  support at all before, so nothing on the SPAWN abi — herd's services, all of
  `box run` — could start a script, and every official OCI image's Entrypoint
  is one.

Tests: `shebang_tests` (host, in `spawn.rs`) for parsing and argv construction;
`spawn_resolves_a_shebang_script` (boot suite) against the real VFS.

## Credentials

There are none. Every process is root and there is no per-process identity to
change:

| Syscall | nr | Behaviour |
|---|---|---|
| `getuid` / `geteuid` / `getgid` / `getegid` | 174-177 | always `0` |
| `getresuid` / `getresgid` | 148 / 150 | write `0` to all three ids; `NULL` pointer → `EFAULT` |
| `getgroups` | 158 | `0` groups; negative size → `EINVAL` |
| `capget` | 90 | full-root set, with real version negotiation |
| `capset`, `setuid`, `setgid`, `setresuid`, `setresgid`, `setgroups` | 91, 146, 144, 147, 149, 159 | accepting no-ops |

**Read the setters' success as "not implemented", not "it worked."** A caller
that asks to become an unprivileged user stays root, silently. That is the same
fiction `getuid` already tells; making them fail instead would not add safety,
only break callers. The real cost is documented under
[`../containers.md`](../containers.md) "Not implemented": a privilege-dropping
entrypoint that re-execs itself loops forever, because the child sees uid 0
again.

`capget` is the one with real logic, and it matters: Linux answers an unknown
`hdr.version` by writing back the version it *does* support and returning
`EINVAL` — a negotiation, which libcap-ng performs by calling `capget` with
version 0 to learn the layout. The old stub returned success for any input,
so every later call used a layout the kernel never agreed to. Note that
libcap-ng reads the *capabilities themselves* from `/proc/self/status`, not
from this syscall — see [`../vfs.md`](../vfs.md) "procfs".

### The capability *number* is bounded, even though the capability *set* is full

`prctl(PR_CAPBSET_READ, cap)` answers 1 for every capability that exists and
`EINVAL` above `CAP_LAST_CAP` (40 — the same bound as the `000001ffffffffff` set
`capget` and `/proc/<pid>/status` report). `prctl(PR_CAP_AMBIENT,
PR_CAP_AMBIENT_IS_SET, cap)` follows the same range rule.

**That rejection is load-bearing, and it is the non-obvious half.** With
`/proc/sys/kernel/cap_last_cap` unreadable — it does not exist on this kernel —
util-linux's `cap_last_cap()` falls back to probing `PR_CAPBSET_READ`. A
kernel that answers every integer makes it conclude `CAP_LAST_CAP` is
`INT_MAX`, and `setpriv --dump` then walks ~2.1 billion capability indices per
set at roughly 0.4 us each: ~13 minutes per set, which presents as a hang, not
as a slow command.

What that hung was `redis:alpine`'s entrypoint, whose privilege-drop gate is
`has_cap() { setpriv -d | grep -q 'Capability bounding set:.*\bsetuid\b'; }` —
so `box run redis:alpine` parked before it ever reached the credential wall
below. Fixed 2026-08-20; boot-suite check `test_prctl_capbset_is_bounded`
(`src/process_tests.rs`). The general shape: **an accepting no-op is only safe
for a setter. For a query that a caller uses to discover a bound, "yes" to
everything is an infinite loop.**

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
- `archive/REDIS_END_TO_END.md` §3-§4 — where the `#!` and credential gaps
  above were found, and why two plausible fixes for the capability failure
  each changed nothing.
