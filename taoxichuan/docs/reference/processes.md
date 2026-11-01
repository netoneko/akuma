# Processes — fork, exec, wait4 and the slot table

**Stability: C** (the fork/preemption path landed recently).

Every process is a fork child of init, through `akuma_exec::process::fork_process`. The child does **not**
run inside the call: the crate publishes it as a runnable thread with its own threading slot, leaked kernel
and trap stacks and a task record, and the arm returns the pid to the caller, as Linux does. Each one
occupies a row of the slot table (`SpawnSlot`, `NSLOTS = 48`). On this kernel **the tid and the row index
are the same number by construction**: `bind_child_task` stamps by tid, and `switch_to`/`task()` index the
same arrays.

Until cleanup step 2e (2026-11-01) a second mechanism coexisted with this one: Akuma's private `SPAWN`
ran a child to completion *inside* the syscall, through a naked-asm crossing on a split syscall stack.
That crossing, `SPAWN`/`WAITPID`/`CLOSE_CHILD_STDIN`, `exec_replace` and the blocking `clone` are deleted
(`docs/archive/TAOXICHUAN_CLEANUP_PLAN.md`); git history before `f821ddda` has the code and its diagram.

## The real fork (slice 3) and the park/wake wait4

```mermaid
sequenceDiagram
    participant Sh as shell (ring 3)
    participant K as kernel
    participant Ch as fork child (own tid, own band)
    participant Id as idle thread
    Sh->>K: fork() (57/58, or clone without CLONE_THREAD)
    K->>K: fork_process (crate): clone address space, capture context<br/>(LIVE fs_base); bind_child_task stamps SpawnSlot,<br/>task record tid+1, counted pipe refs, out_slot/depth
    Note over K: stamp_child_parent(child pid → shell pid)
    K-->>Sh: child pid — the child is RUNNABLE, not running
    Note over Ch: runs when the pick loop/tick reaches it;<br/>resumes the parent's context with RAX = 0
    Sh->>K: wait4(-1, …) — no dead child of MINE yet
    Note over K: wait4_real: stamp slot.waiter = my tid,<br/>schedule_blocking(∞) — park
    K->>Id: core goes to the idle thread (halt laps)
    Ch->>K: exit_group → task_exit_real
    Note over K: status into the slot, pipe refs dropped,<br/>waiter woken through the threading waker,<br/>x86_finish_current (never returns)
    K->>Sh: wait4 re-scans, reaps MY child, returns pid + status
```

- The fs base is **captured live** from the forking parent's
  `IA32_FS_BASE` (`capture_user_context` runs in the parent's context, before
  any switch) — a fork child inherits its parent's TLS, the same answer a real
  fork gives. `twin_enter_user` installs `ctx.fs_base` on every entry.
- **A thread owns a `Process` row, and its exit must retire it.** `clone_thread` registers a real `Process` per thread. Only `reap_process` (a group *leader*, on its parent's `wait4`) used to unregister one, so a thread's own `exit` and `exit_group`'s `kill_siblings` leaked a process-table entry each; a `cargo build` of this kernel reached `Process table full` at pid ~345. Now `retire_thread_process` runs on both paths and `clone_thread_arm` calls `drain_retired()` at syscall *entry* (never at teardown: reclaim runs `Process::drop` and the exiting thread is still on its row) - the amd64 kernel's fix for the same family. `[reap]` lines print `procs=<active>/<retired>`; the count plateaus (peak 30 over the whole build). [`archive/TAOXICHUAN_BUGS_FOUND_2026_10_30.md`](../../../docs/archive/TAOXICHUAN_BUGS_FOUND_2026_10_30.md) §7.
- The fd table is `akuma-exec`'s `SharedFdTable`: `fork_process` deep-clones
  it, with each descriptor kind's reference rules (pipe ends, sockets), so
  `bind_child_task` copies no fds.
- There is no fork depth ceiling: a fork child owns a threading stack and a
  syscall band of its own. Only the boot task (no registered process) is
  refused (`c1c885c5`; sshd -> session -> sh -> cargo -> rustc -> cc ->
  collect2 -> ld is eight deep).
- The `real` bit on the slot is set only by `bind_child_task`; an exit routes
  to `task_exit_real` only when it is set.

## wait4 is parent-owned

`PARENT_OF` is a 32-entry pid→pid ring, stamped at `fork_arm_real` (keyed by
**pid, never slot index** — slots and tids are reused across successive
children; pids are monotonic). It pulls the akuma-exec crate's
`is_child_of_group` decision into this kernel's slot scans: **wait\* reaps
only children registered to the caller's own pid, answers ECHILD for a
stranger's, and never parks on one.**

- `wait4_real` (slotted callers): the reap scan skips rows stamped to another
  parent; the live scan parks only on *my own* running children; a childless
  `wait4(-1)` answers ECHILD. Unstamped rows (`po == 0`) are **nobody's
  children** here — the spawn model's finished shells sit in the table as
  zombies the sshd bridge never waits for, and pj's rule keeps any later
  `wait4(-1)` from reaping them (grandfork step 5's exit-5 bug).
- `wait4_arm_blocking_scan` (the boot task, which owns no stamped children
  and may not park): skips *stamped* rows, keeps unstamped visibility — the
  M-gates and the bridge's reaper do not move. An orphan whose parent died is
  unreapable here: the crate reparents to init, and this kernel has no init
  reaper — the honest no-init answer (one slot row per abandoned grandchild,
  harmless at the 48-slot ring).
- A second wait on a reaped pid is ECHILD (how a shell's job table closes the
  entry). WNOHANG answers 0 while the matched children still run.
- Across the park, the task's user RSP is saved and restored by hand: the
  stub's stash is a **global** every task's syscall entry overwrites.
- Akuma-private `WAITPID` (0x1000+303) and `CLOSE_CHILD_STDIN` (0x1000+326)
  are unchanged: the bridge's poll (0 = "still running") and a no-op success.

## The fs discipline (per-slot TLS shadow)

`IA32_FS_BASE` is per-**core**, and this kernel multiplexes every task onto
one core — so fs is carried per-slot state (the `fs` field of the slot's user record in `akuma-sched-amd64`'s `Machine` table, `sched::set_slot_fs`; it was a separate `SLOT_FS` array until A5):

- `switch_to` installs the incoming task's shadow (one wrmsr per switch; the
  idle thread is skipped — kernel-only, no TLS).
- `arch_prctl(ARCH_SET_FS)` and every crossing keep the shadow in step.
- **Exec zeroes fs** (`exec_real_task`, and the boot spawn hands init a
  deterministic `fs = 0`): the MSR rides the
  `sysretq` crossing untouched, so without the zero a depth-2 exec entered the
  new image with the *outgoing* task's TLS pointer — grandfork's first
  stack-guard read (`fs:0x28`) hit the exec'd shell's busybox bss, unmapped in
  the fresh CR3, and the task died before musl could run its own arch_prctl
  (boot ph-7's PK2). The fork path's inheritance is deliberate (same image);
  the exec path's is not (a new image — 0 is what a depth-1 exec gets from
  the boot crossing and what the amd64 kernel hands its images).

## The runtime twin

`src/exec_twin.rs` registers the four hook tables the shared crates read —
`ExecRuntime` + `ExecConfig` (`akuma_exec::runtime::register`) and
`ThreadRuntime` + `ThreadConfig` + `ProcessHooks` + `X86ArchHooks`
(`akuma_threading`) — the twin of amd64's `exec_runtime.rs::init`. Every arm
either names a taoxichuan module that already exists (pipes, sockets, the ext2
root, COM1) or is a no-op **with the reason stated** (the
`AKUMA_AMD64_STALE_FALSE_HOOKS.md` lesson). The slice-3 arms are real now:
`switch_to`, `bind_child_task` (`child::bind_child_task` behind the hook),
`enter_user` (`twin_enter_user`, installs `ctx.fs_base` and stamps the slot
shadow). The one-clock rule: `uptime_us` is `akuma_lapic_amd64::ticks()` ×
10 ms for both tables, so the scheduler and `akuma-exec` cannot disagree about
what time it is.

## Session ids, the execve argument contract, and the fd floor

- **Every task is its own session and group leader**: `setsid` succeeds and
  returns the pid, `getsid`/`getpgid` answer the pid, `setpgid` accepts and
  does nothing worth recording — there is no job control. `git` daemonises
  with `setsid` and used to die with "Function not implemented" (112 served
  2026-10-30). `getppid` is 1, `gettid` is the threading slot.
- **The `execve` argument block** (`read_spawn_args`,
  [`memory.md`](memory.md) § allocation): 448 `argv` / 256 `envp` entries, one
  string up to 32 KiB, `argv + envp <= 512`, and a total-bytes guard —
  every overrun answers `E2BIG`, as Linux's does; a non-UTF-8 argument still
  answers `EFAULT` because the loader path carries `&str`. One heavy `rustc`
  call carries 344 arguments and a 5,192-byte string; the loader's argument
  block is 12 pages.
- **The fd floor honours closed std fds**: a task that `close(0)`s and has not
  re-bound the slot remembers it (`Task.std_closed`, copied on fork), and
  `new_fd_min()` hands the lowest closed std fd out first, else 3 — so
  `close(0); open("/dev/null")` returns 0, which busybox ash's every
  background job (`cmd &`) depends on. `open`, `socket`, `accept`, `pipe2` and
  `socketpair` all allocate from there; a re-bound std fd closes like any
  other fd.

## Background

`docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md` (the model),
`docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md` (the twin's slices),
`docs/archive/TAOXICHUAN_M4_SLICE3_PE_PLAN.md` …
`docs/archive/TAOXICHUAN_M4_SLICE3_PH_RESULT.md` (the slice-3 progression:
pipeline deadlock root causes, the real fork, parked wait4),
`docs/archive/TAOXICHUAN_M4_SLICE3_VERDICT_PLAN.md`,
`docs/archive/AKUMA_AMD64_STALE_FALSE_HOOKS.md` (the false-hook lesson),
`docs/archive/KVM_RELAY_SYN_STALL.md` (the shared-host-core constraint).


## Threads, pids and faults (2026-10-30)
- **Threads.** `clone(CLONE_THREAD)` calls `akuma_exec::process::clone_thread`, which publishes a runnable slot through the same bind hook as `fork`; for a thread the hook records `TASK_OF[slot]` = the creator's task record, so regions, brk/mmap cursors, the fd table and the cwd are one copy. `exit` (60) from a thread zeroes and wakes its `clear_child_tid` word (what `pthread_join` waits on) and ends that thread; `exit_group` ends the whole group, and a thread calling it hands the status to the group leader's waiter. A thread is `SpawnSlot.thread = true` and never appears as a reapable child.
- **One pid allocator.** Every pid, fork child or thread, comes from `akuma_exec::process::allocate_pid` (floor 2). A private counter here once issued the same pid as a thread and collapsed two table rows.
- **Faults.** A ring-3 fault in a fork child or thread ends that task alone with wait status SIGSEGV (`rc=139`); the boot task still halts (it has no slot to reap through).

## Init is pid 1 on a pool thread

The first boot program (`init=`) does not run on the boot stack. `spawn()` loads it, then `child::start_init_thread` claims a pool slot
(`x86_claim_slot`, tid >= 1), stamps its ring row and task record the way a fork child's are (`bind_child_task`, record `tid + 1`, depth
1), registers a `Process` with pid 1 (`register_shell_process`, so `fork_process` finds a caller), seeds the thread to cross into ring 3
at the image's entry and publishes it. The boot thread is adopted as pool slot 0 (`x86_adopt_running_thread(0)`, as amd64 does) and
parks in a loop that reports init's exit through `task_done`. Consequences: init can `fork`/`clone` (every non-thread `clone` is
`fork_arm_real`) and is preempted by the tick, so `sshd` built with `fork-sessions` + `libakuma/linux-abi` runs one forked process per ssh
session. A cooperative sshd (Akuma's private `SPAWN`) gets `ENOSYS`: that syscall no longer exists here. The fresh thread names its own slot current
before crossing (`init_thread_entry`): the crate does not do it for a seeded entry. Slice S1 of `switch-to-crates.md`.
