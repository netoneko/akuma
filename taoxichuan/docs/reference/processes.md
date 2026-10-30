# Processes — the child model, the crossing, and the spawn ring

**Stability: C** (the M4 slice-3 real-fork/preemption path landed recently;
the M2 blocking arm still serves `clone`, and both shapes share one ring).

Two process mechanisms coexist, both indexed by the same spawn ring
(`SpawnSlot`, `NSLOTS = 48` — the shell's slot 0 plus 47 threading tids;
the ring was 12 until the first `cargo build` filled it, now 48:
on this kernel **the tid and the ring index are the same number by
construction**: `bind_child_task` stamps by tid, `switch_to`/`task()` index
the same arrays):

1. **The M2 spawn/blocked-fork model** — a child runs to completion *inside*
   the SPAWN (or blocking-fork) syscall. Its stdout lands in a kernel buffer
   (the "pipe" — there is no reader to block, so a `Vec<u8>` is the whole
   semantics); SPAWN returns the pid and a `ChildOut` fd; the exit status is
   recorded and `waitpid`/`wait4`-able from the moment SPAWN returns.
2. **The real fork** (`REAL_FORK = true`): plain `fork`/`vfork` (57/58) go
   through `akuma_exec::process::fork_process` over the shell's registered
   `Process`. The child does **not** run inside the call — the crate
   publishes it as a runnable thread with its own threading slot, leaked
   kernel+trap stacks and task record, and the arm returns the pid to the
   caller, exactly like Linux. `clone` keeps the M2 blocking arm.

## The SPAWN crossing (M2 model)

```mermaid
sequenceDiagram
    participant P as parent (sshd, ring 3)
    participant K as kernel syscall path (IF=0)
    participant C as child (ring 3)
    P->>K: SPAWN(path, argv, envp, …)
    Note over K: marshal args; reserve ring slot in the threading pool<br/>(FREE or TERMINATED, NSLOTS tries, never blind-next);<br/>load_task on the PARENT's CR3 (all writes via physmap)
    Note over K: slot record ← parent CR3, syscall-stack top,<br/>user-rsp stash, depth=1, out_slot=slot;<br/>register shell Process; PREEMPT_ARMED = true
    K->>K: set_current_slot(slot); lower syscall-stack top by half;<br/>fs = 0 (a fresh image never inherits the outgoing TLS)
    K->>C: child_enter (naked): save parent callee-saved regs + rsp<br/>into the slot's CrossSave, mov cr3, sysretq
    loop child syscalls
        C->>K: syscall → dispatch on the nested stack band
        K-->>C: sysretq with the answer
    end
    C->>K: exit_group(code)
    Note over K: child_exit: record status (code<<8) in the slot,<br/>release the task's pipe ends, set_current_slot(parent)
    K->>K: exit_cross (naked): restore regs from CrossSave,<br/>mov cr3 (parent), mov eax, status, ret
    K-->>P: SPAWN returns pid | stdout_fd<<32
```

Canonical mermaid source: [`../diagrams/process-model.md`](../diagrams/process-model.md).

- `child_enter` and `exit_cross` are **naked** functions: their whole bodies
  are the register contract. The inline-asm version of the exit tail once let
  LLVM allocate an input onto a register the template itself overwrote —
  `rbx`/`rbp` cannot be explicit inline-asm operands at all, which closed the
  inline form off entirely (child.rs documents the incident on the function).
- The syscall stack is split: the M2 child dispatches on the **lower half** of
  the parent's 64 KiB band (a blocking-fork child at a quarter below the
  shell's top). Real fork children get their own leaked 64 KiB band via
  `prepare_task_slot`.
- `CrossSave` is `repr(C)`, addressed by offset from asm (0x00 rsp, 0x08 rbx,
  … 0x30 r15). Per-slot records (`SLOT_STATE`) replaced the shared crossing
  statics: "who am I" is `exec_twin::current_slot()`, the one sanctioned
  current-task static; the SMP slice puts `%gs` behind it and changes no
  caller.
- **The ring slot is reserved in the threading pool before anything
  expensive** (`akuma_threading::reserve_slot`: FREE→INITIALIZING, accepting
  TERMINATED so a dead fork child's number can be reclaimed, NSLOTS tries,
  never the blind-next wrap onto a live ring). An unreserved number used to be
  handed to a fork child, which then bound the shell's own cross record — the
  pipeline deadlock of boots ph-2/ph-4.
- The exec'd shell is registered as an `akuma-exec` `Process`
  (`register_shell_process`, amd64's bootstrap registration adapted field for
  field): `fork_process` opens with `current_process_shared()`, so the shell
  must be in the process table and the tid↔pid map before it can fork. Its
  address space is a `new_shared` **view** — the twin's task record owns the
  root; a second owner would double-free it. `exec_replace`/`exec_real_task`
  re-point the view at the fresh root after the image swap.
- The first SPAWN crossing arms timer preemption (`PREEMPT_ARMED`) — the
  first moment a ring-3 tick has a legitimate switch target
  ([`scheduling.md`](scheduling.md)).

## The real fork (slice 3) and the park/wake wait4

```mermaid
sequenceDiagram
    participant Sh as shell (ring 3)
    participant K as kernel
    participant Ch as fork child (own tid, own band)
    participant Id as idle thread
    Sh->>K: fork() (57/58, REAL_FORK)
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
- The fd table clone in `bind_child_task` takes the **counted pipe
  references** with it (one `clone_ref` per cloned `Pipe` payload — the M2
  arm's own rule; without the walk, the first post-fork close destroyed the
  pipe under two live cloned fds).
- A fork child's fd-1 writes land in the buffer sshd drains (`out_slot`
  inherited), never its own slot; depth is stamped parent+1.
- Fork depth ceiling is **3** (the shell at 1, its children 2, grandchildren
  3): under the real path a fork child owns a threading stack and a syscall
  band of its own, so the M2 "no stack region left" ceiling no longer binds.
  Deeper stays `EPERM`.
- The `real` bit on the slot is the discriminator between a real fork child
  (set only by `bind_child_task`) and an M2 spawn child (same
  used/running/current triple): an exit routes to `task_exit_real` only for
  the former.

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
one core — so fs is carried per-slot state (`SLOT_FS[NSLOTS]`):

- `switch_to` installs the incoming task's shadow (one wrmsr per switch; the
  idle thread is skipped — kernel-only, no TLS).
- `arch_prctl(ARCH_SET_FS)` and every crossing keep the shadow in step.
- **The exec crossings zero fs** (`exec_replace`'s and `exec_real_task`'s, and
  the spawn crossing hands images a deterministic `fs = 0`): the MSR rides the
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
- **Faults.** A ring-3 fault in a real fork child or thread ends that task alone with wait status SIGSEGV (`rc=139`); the shell and the boot task still halt (they have no slot to reap through).
- **The spawn crossing marks its pool slot RUNNING around `child_enter` and TERMINATED after.** Without it the picker never takes the slot (`INITIALIZING`), and the first preempting tick parked the spawn forever (deterministic under TCG).
- **Sockets** are closed by the last fd referencing them across all task records (`sock::socket_refs`); an exiting task releases its sockets.
- **Known limit:** enough spawns in one boot leak a kernel+trap band each until a claim picks a slot at or above the ring (`"Failed to allocate x86 task stacks"`); the 12-slot ring failed after ~15 spawns, the 48-slot ring scales the same leak (not re-measured). It dies with the ring in `switch-to-crates.md` S3.
