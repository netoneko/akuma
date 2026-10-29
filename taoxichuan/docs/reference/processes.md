# Processes — the child model, the crossing, and the spawn ring

**Stability: C** (the M4 slice-3 work — real `fork_process` over
`akuma-exec`/`akuma-threading` — is in flight; the eager blocking fork below
is marked THROWAWAY in the source).

There is no scheduler and no second runnable task. The one design decision of
the stage (`src/child.rs`): **a spawned child runs to completion inside the
SPAWN syscall.** Its stdout lands in a kernel buffer (the "pipe" — there is no
reader to block, so a `Vec<u8>` is the whole semantics); SPAWN returns the pid
and a `ChildOut` fd; the exit status is recorded and `waitpid`/`wait4`-able
from the moment SPAWN returns. At every instant either the parent is in the
kernel running the child, or the parent is in ring 3 and the child does not
exist.

## The crossing

```mermaid
sequenceDiagram
    participant P as parent (sshd, ring 3)
    participant K as kernel syscall path (IF=0)
    participant C as child (ring 3)
    P->>K: SPAWN(path, argv, envp, …)
    Note over K: marshal args; claim spawn-ring slot + pid;<br/>load_task on the PARENT's CR3 (all writes via physmap)
    Note over K: slot record ← parent CR3, syscall-stack top,<br/>user-rsp stash, depth=1, out_slot=slot
    K->>K: set_current_slot(slot); lower syscall-stack top by half
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
- The syscall stack is split: the child dispatches on the **lower half** of
  the parent's 64 KiB band (a fork child at a quarter below the shell's top).
  The parent frames live a few hundred bytes below the top; the bands cannot
  meet.
- `CrossSave` is `repr(C)`, addressed by offset from asm (0x00 rsp, 0x08 rbx,
  … 0x30 r15). Per-slot records (`SLOT_STATE`) replaced the shared crossing
  statics: "who am I" is `exec_twin::current_slot()`, the one sanctioned
  current-task static; the SMP slice puts `%gs` behind it and changes no
  caller.

## exec-replace

In a run-to-completion model, `execve` from the nested child **is** the
handoff: busybox ash's tail-call exec of the one command in `sh -c` (no fork).
`exec_replace` snapshots the slot's `CrossSave` into a frame local (deeper
runs write strictly below it), loads the new image over the same slot, restores
the fd table across the load (POSIX keeps open fds; the cloexec sweep releases
swept sockets and pipe ends exactly as `close` does), lowers the nested
dispatch top below the live frame, crosses again — and the exec'd image's
`exit_group` returns into `spawn_arm` as if its own child had finished. Its
stdout lands in the same pipe; its status is the child's.

`execve` outside a nested child is `ENOSYS`: there is no task this kernel
could replace.

## The blocking fork (M2 scaffold)

`clone`/`fork`/`vfork` (56/57/58) run the child to completion **inside the
fork call**: the child resumes the caller at its own RIP with every user
register it had (`RAX` forced to 0) on an eager, flag-faithful copy of its
memory ([`memory.md`](memory.md) § fork). Rules:

- Fork lives only inside the spawned shell (depth 1). The boot task and a
  fork child (depth 2, no stack band left) get `EPERM`.
- `clone` flags accepted: `SIGCHLD` alone, or `SIGCHLD|CLONE_VM|CLONE_VFORK`
  (posix_spawn's shape — vfork's contract holds by construction: the parent
  does not resume until the child has exited). Anything else is `EINVAL` with
  the flags printed. Raw `fork`/`vfork` (57/58) carry no flags word.
- The child's fd table is `akuma-fdtable`'s own `fork_copy` decision, never a
  hand copy; counted pipe ends get one `clone_ref` per inherited payload.
- The child reuses a **dead** spawn-ring slot, never the ring's blind next —
  a four-fork pipeline otherwise wraps the ring onto the live shell's slot and
  wipes the session buffer.
- A fork child's fd-1 writes land in the buffer sshd drains (`out_slot`
  inherited), never its own slot.

## The spawn ring and the waits

Four `SpawnSlot`s, recycled; pids from 2 up (1 is the boot task). A slot
records `pid`, `status` (Linux wait status, `code << 8`), the stdout buffer,
`reaped`.

- `wait4(pid, &status, …)`: reaps the first used, un-reaped, non-running slot
  matching the pid (−1/0 = any); a second wait on a reaped pid is `ECHILD`,
  which is how a shell's job table closes the entry. `WNOHANG` on a running
  child would answer 0 — unreachable in this model (a child completes inside
  its own fork call), answered honestly anyway.
- Akuma-private `WAITPID` (0x1000+303): the bridge's poll — pid with status
  when reaped, **0 for an unknown-but-plausible pid** (the ABI's "still
  running", not an error).
- `CLOSE_CHILD_STDIN` (0x1000+326): success without effect — the buffer model
  has no reader to signal, so EOF was "delivered" from the bridge's point of
  view.
- `SPAWN` (0x1000+301) returns `pid | stdout_fd << 32`, the same packed word
  the full kernel returns. A spawn that could not hand out the fd reports fd 0
  rather than lying about a child that did run.

## The runtime twin

`src/exec_twin.rs` registers the four hook tables the shared crates read —
`ExecRuntime` + `ExecConfig` (`akuma_exec::runtime::register`) and
`ThreadRuntime` + `ThreadConfig` + `ProcessHooks` + `X86ArchHooks`
(`akuma_threading`) — the twin of amd64's `exec_runtime.rs::init`. Every arm
either names a taoxichuan module that already exists (pipes, sockets, the ext2
root, COM1) or is a no-op **with the reason stated** (the
`AKUMA_AMD64_STALE_FALSE_HOOKS.md` lesson). The arms that land with
`fork_process` (slice 3) — `enter_user`, `fork_share_memory`,
`bind_child_task` — panic/`Err` naming their slice. The one-clock rule:
`uptime_us` is `akuma_lapic_amd64::ticks()` × 10 ms for both tables, so the
scheduler and `akuma-exec` cannot disagree about what time it is.

## Background

`docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md` (the model, incl. the
owner-approved blocking-fork exception),
`docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md` (the twin's slices),
`docs/archive/TAOXICHUAN_M4_SLICE3_PH_RESULT.md` (what is green, what is
open), `docs/archive/AKUMA_AMD64_STALE_FALSE_HOOKS.md` (the false-hook
lesson).
