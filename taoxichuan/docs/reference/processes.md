# Processes — fork, exec, exit, wait4 and the slot table

**Stability: B** (the model is `akuma-exec`'s and glue's; this kernel's part is the machine state and the
address-space lifetime; verify against a current battery run).

Every process is a fork child of init, through `akuma_exec::process::fork_process`. The child does **not**
run inside the call: the crate publishes it as a runnable thread with its own threading slot, and the arm
returns the pid, as Linux does. Each live task occupies one row of `child.rs`'s slot table (`SpawnSlot`:
`used`, `pid`, `running`; `NSLOTS = 48`). On this kernel **the tid and the row index are the same number by
construction**: `bind_child_task` stamps by tid, and `switch_to` and the task records index the same arrays.
The per-slot machine state (kernel and trap stacks, the saved user context, the fs shadow) is
`akuma-sched-amd64`'s `Machine` table, which `sched.rs` instantiates.

## Init is pid 1 on a pool thread

The first boot program (`init=`) does not run on the boot stack. `spawn()` loads it, then
`child::start_init_thread` claims a pool slot (`claim_slot`, tid >= 1), binds it the way a fork child is
bound, registers a `Process` with pid 1 (`register_shell_process`, so `fork_process` finds a caller), seeds the
thread to cross into ring 3 at the image's entry, and publishes it. The boot thread is adopted as pool slot 0
(`adopt_running_thread(0)`, as amd64 does) and parks, reporting init's exit through `task_done`. The fresh
thread names its own slot current before crossing (`init_thread_entry`); the crate does not do that for a
seeded entry.

## fork, threads, exec, exit and wait4

```mermaid
sequenceDiagram
    participant Sh as shell (ring 3)
    participant K as kernel
    participant G as glue
    participant Ch as fork child (own tid)
    Sh->>K: fork() (57/58, or clone without CLONE_THREAD)
    K->>K: fork_process (akuma-exec): new Process, fd table deep-cloned,<br/>context captured (live fs_base); fork_share_memory = clone_user_half (CoW);<br/>bind_child_task stamps the slot and the task record
    K-->>Sh: child pid (the child is RUNNABLE, not running)
    Note over Ch: runs when the picker reaches it;<br/>resumes the parent's context with RAX = 0
    Sh->>G: wait4(-1, …)
    Note over G: parks on the child channel
    Ch->>K: exit_group → task_exit_code
    Note over K: shared mappings written back, siblings stopped,<br/>fds closed, root to the zombie table;<br/>publish_child_exit wakes the parent, SIGCHLD
    G->>Sh: reaps the zombie: pid + status
    Note over K: after_reap = child::collect frees the root
```

- **Clone flags** decode in `akuma-syscalls-proc::clone::decode`: `CLONE_THREAD` → `clone_thread_arm`
  (`akuma_exec::process::clone_thread`), a `posix_spawn` clone with a child stack → `fork_arm_real(stack)`,
  everything else (and `fork`/`vfork`, accepted bare) → `fork_arm_real(0)`. Only the boot task, which has no
  registered process, is refused (`EPERM`). There is no fork depth ceiling.
- **A thread** shares its creator's task record (`TASK_OF[slot]` names the creator's): regions, `brk`/mmap
  cursors and the cwd are one copy, and the fd table is the process's. `exit` (60) from a thread zeroes and
  wakes its `clear_child_tid` word and ends that thread; `exit_group` ends the whole group.
- **A thread owns a `Process` row, and its exit retires it** (`retire_thread_process`); `clone_thread_arm`
  drains retired rows at syscall *entry*, never at teardown, because reclaim runs `Process::drop` while the
  exiting thread is still on its row.
- **exec** (`child::exec_real_task`): the argument rules are `akuma-syscalls-proc`'s (below), the load is `load_task` ([`memory.md`](memory.md)), and the commit
  (`/proc/self/exe`, close-on-exec, caught handlers reset) is glue's `execve::commit`, shared with amd64. The
  fds survive; the thread keeps running on the new image.
- **exit** (`task_exit_code`): writes shared file mappings back, stops every sibling thread
  (`kill_siblings`), closes the fds *before* the reap (a pipe's EOF is its last writer's close, so a
  pipeline's reader cannot wait for its writer's parent to call `wait4`), hands the page-table root to the
  zombie table, and publishes the exit the way `akuma-exec` publishes every exit (`publish_child_exit`: the
  child channel is marked, a parent parked in glue's `wait4` wakes, SIGCHLD is raised).
- **wait4 and waitid are glue's**, over `akuma-exec`'s child channels, on the allow-list's `process` group
  ([`syscalls.md`](syscalls.md)). After each one, `child::collect` frees the roots of reaped pids, drains
  retired rows and sweeps parked roots.
- **A death inside glue** (a fatal default action, such as SIGPIPE) marks the thread TERMINATED from
  `akuma-exec`; the slot-purge hook (`child::on_thread_terminated`) purges its futex waits and, for a
  process's last thread, moves its root to the zombie table.
- **A ring-3 fault** in a fork child or thread ends that process with wait status SIGSEGV (`rc=139`); the boot
  task still halts, having no slot to reap through.

## Address-space lifetime

A fork child's address space is `clone_user_half`'s copy-on-write clone, whose frames the `Process`'s
`UserAddressSpace` ledger never saw. So this kernel frees roots itself and the `Process` must not:
`disown_space` hands the `Process` a non-owning view of the root and forgets the owning one. A process's
current root is in `OWNED_ROOT` (its exec'd image) or its task record (never exec'd); at exit it moves to
`ZOMBIE_ROOT` (64 entries), and `collect` parks it for freeing once glue's `wait4` has unregistered the pid. A
full table leaks the image and says so (`[reap] zombie-root table full`) rather than freeing a root a view
still names. The gate is `scripts/kvm/goose/cow_leak_gate.py` ([`memory.md`](memory.md) § fork's memory).

## The fs discipline (per-slot TLS shadow)

`IA32_FS_BASE` is per-**core** and tasks move between cores, so fs is per-slot state: the `fs` field of the
slot's user record in `akuma-sched-amd64`'s `Machine` table (`sched::set_slot_fs`).

- `switch_to` installs the incoming task's shadow (one wrmsr per switch; idle threads are skipped).
- `arch_prctl(ARCH_SET_FS)` and every crossing keep the shadow in step.
- **A fork child inherits its parent's fs**: `capture_user_context` reads the live MSR in the parent's
  context, before any switch.
- **exec zeroes fs**, and init starts with `fs = 0`: the MSR rides the `sysretq` crossing untouched, so without
  the zero an exec'd image ran with the outgoing image's TLS pointer and died on its first stack-guard read
  (`fs:0x28`) before musl could set its own.

## The runtime twin

`src/exec_twin.rs` registers the hook tables the shared crates read: `ExecRuntime` + `ExecConfig`
(`akuma_exec::runtime::register`) and `ThreadRuntime` + `ThreadConfig` + `ProcessHooks` + `ArchHooks`
(`akuma_threading`; the arch hooks are `akuma_sched_amd64::hooks::<sched::Tx>()`). Every arm either names
code that exists here or is a no-op **with the reason stated** (the `AKUMA_AMD64_STALE_FALSE_HOOKS.md`
lesson). One clock: both tables get `net::uptime_us` (the calibrated TSC), the same clock
`akuma_primitives::clock`, futex deadlines and smoltcp read, so none of them can disagree about what time it
is.

## The execve argument contract

Glue's `execve::read_request` reads the arguments, charging each string to an `akuma-syscalls-proc`
`exec::Budget`: 448 `argv` / 256 `envp` entries, one string up to 32 KiB, `argv + envp <= 512`, and a total that
fits the 12-page argument block. Every overrun answers `E2BIG`, as Linux's does. A non-UTF-8
argument answers `EFAULT`, because the loader path carries `&str`. One heavy `rustc` call carries 344
arguments and a 5,192-byte string.

## Background

[`TAOXICHUAN_CLEANUP_PLAN.md`](../../../docs/archive/TAOXICHUAN_CLEANUP_PLAN.md) (steps 2d-2f, A1-A5: the private
crossing, `SPAWN` and the private `wait4` deleted; one slot table),
[`TAOXICHUAN_SWITCH_TO_CRATES.md`](../../../docs/archive/TAOXICHUAN_SWITCH_TO_CRATES.md) (the decision),
`docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md` (the model),
`docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md` (the twin's slices),
`docs/archive/TAOXICHUAN_M4_SLICE3_PE_PLAN.md` … `docs/archive/TAOXICHUAN_M4_SLICE3_PH_RESULT.md` (the real
fork, the parked wait, the exec fs zero),
[`TAOXICHUAN_BUGS_FOUND_2026_10_30.md`](../../../docs/archive/TAOXICHUAN_BUGS_FOUND_2026_10_30.md) §7 (the
thread-row leak), `docs/archive/AKUMA_AMD64_STALE_FALSE_HOOKS.md`.
