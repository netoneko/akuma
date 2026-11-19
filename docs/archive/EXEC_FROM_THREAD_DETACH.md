# `execve` from a non-leader thread: the panic (E7), how to reproduce it, and the detach fix (2026-11-19)

Branch `ecosystem-demo-stage1`. Kernel: the main amd64 kernel (`crates/akuma-kernel-amd64`). Found by the ecosystem demo's
stage 2 (`docs/archive/AKUMA_ECOSYSTEM_DEMO.md`, `proposals/FUTURE_WORK.md` E7).

## Symptom

`scala-cli run` of the task board, in a guest booted by `overlays/ecosystem-demo/refdemo.py`, panics the guest kernel:

```
  [execve] from a non-leader thread with live siblings; they are not killed (no leader transfer on this target)
  #PF: not-present read from ring 0
  rip=0xffffffff8041d269 ... cr2=0x00007f53f000f020      (akuma_exec::process::Process::with_address_space::<(), sys_mmap::{closure#3}>)
```

A program without the kernel panic shows the same defect as a SIGSEGV in the *old* image's other threads
(`[Fault] #PF page fault in ring 3 ... err=0x14`, instruction fetch from an address the old space mapped).

## Cause

This kernel keeps a process's address space in its **thread-group leader's** `Process`. `sys_execve` resolves its target
through `current_thread_tgid_process()`, so an `execve` made by any thread replaces the **group's** space. On Linux the
exec first ends every other thread of the group (`de_thread`) and the caller takes over the leader's identity; here
`ExecArch::stop_siblings` drains only when the caller *is* the leader, and from a non-leader it prints the line above and
carries on. The siblings (and the leader) then run on frames the replacement freed: a ring-3 fetch fault, or, when a
sibling is inside `sys_mmap` and walks the replaced space's bookkeeping, a ring-0 `#PF`.

Who calls `execve` from a thread: the JVM running scala-cli (a Java thread; the exact caller was not identified; the
kernel line did not name the program). Denver does not have the problem because its exec gives only the calling task a
new root and never touches a group-wide space.

## How to reproduce

Probe (static musl, no extra dependencies): `userspace/forktest/execthread/execthread.rs`. It starts four threads that keep
allocating and entering the kernel, then calls `Command::exec` from a fifth.

```sh
rustc --edition 2021 -O --target x86_64-unknown-linux-musl -C target-feature=+crt-static \
      userspace/forktest/execthread/execthread.rs -o /tmp/execthread
# boot any amd64 guest that has a shell (e.g. the demo's guest 1: `overlays/ecosystem-demo/launch.py up`,
# kernel = target/x86_64-unknown-none/release/akuma built with --no-default-features --features headless,no-tests),
# copy the probe in and run, over ssh:
/tmp/execthread ok /bin/sh -c 'echo NEWIMAGE'     # exec from a thread, expect NEWIMAGE
/tmp/execthread fail                               # exec of a missing path from a thread, expect ENOENT and MAIN-ALIVE
```

Before the fix, `ok`: `[execve] from a non-leader thread ...` in the serial log, then a user fault
(`rip=... err=0x14`, `Segmentation fault`, rc=139) in the old group. The ring-0 panic needs a sibling to be inside
`sys_mmap` at the moment of the replacement, which the JVM does constantly and the probe only sometimes; rerun, or add
workers. The full reproduction is the demo itself: `overlays/ecosystem-demo/refdemo.py --ref ../akuma-demo-reference-impl`
(about 10 minutes; the panic comes after the scala-cli compile passes, when `scala-cli run` starts the board) and the guest
serial log, `$ECO_WORK/guest1-panic.serial.log` (the launcher keeps it).

## Fix: detach (Denver-style), `usermode::sys_execve_detached`

An `execve` from a non-leader thread **with other threads alive** (`live_count(tgid) > 1`; a lone non-leader still takes
the old path, which worked) becomes a **new process**:

1. The calling thread forks (`fork_process`, copy-on-write, own thread group, own copy of the descriptor table). A slot in a
   fixed table (`DETACH`, keyed by the child's pid, registered *before* the fork so the child cannot run first) marks the child.
2. The child's first entry (`run_process`) runs the parent's `execve` as an ordinary leader: the arguments are the registers the
   fork copied, the strings are in its copy of the parent's memory. On success it enters the new image directly (there is no
   syscall return to carry `exec_pending`, so the entry is taken from the task's `UserCtx` and the flag is spent). It reports the
   verdict through the table and wakes the caller.
3. Caller, on success: the old image is gone, so the **whole old group ends** (`exit_group` from a thread, status 0). On failure:
   it returns the errno, with its own state untouched, as `execve` does.

What Denver contributed is the idea, not code: its exec builds a root for the calling task only, which does not exist on this
kernel (the space is the group's). Nothing in the fix touches the leader's `Process`.

Divergences from Linux, deliberate:

* the exec'd program has a **new pid**; a parent `wait4`ing the old pid is told the old image ended (status 0) at the exec, not
  when the new program exits; a foreground `scala-cli run` returns early;
* the new program is a child of the exiting thread (not checked: where it is reparented when the old group ends);
* the caller's descriptor table is copied (fork semantics), so close-on-exec closes on the copy, as on Linux, and
  the old group's descriptors are closed when the old group ends.

Allocation: none (fixed `DETACH` table, `WakeHandle` is a value). Console output uses `serial::puts` as the file does.

## Verified

* old kernel, probe `ok`: `Segmentation fault`, rc=139, and the `[execve] from a non-leader thread` line.
* new kernel, probe `ok`: the new image's output, the old group gone, the ssh session returns; probe `fail`: `ENOENT`,
  `EXECTHREAD-MAIN-ALIVE` (the failure path leaves the caller and its group intact).
* the demo: `overlays/ecosystem-demo/refdemo.py --ref ../akuma-demo-reference-impl` on the fixed kernel: the board starts, the
  reference acceptance suite passes from guest 2, 19 ok/info, 0 failed (before: guest 1 kernel panic).

## Future work: the Linux-style handler (`proposals/FUTURE_WORK.md` E7b)

Detach changes pids and wait semantics. The faithful fix is `de_thread`:

1. `stop_siblings` for a non-leader caller: set the group's exit flag, `request_thread_kill` + wake every other thread **and the
   leader**, and wait for them (the existing `drain` counts non-leaders only, and excludes the caller by pid, so it needs a
   "everyone but me" form; the leader has no `Process`-preserving way to die today: a leader's `exit` is a process exit that
   runs `finish_process`, publishes the status and frees the space).
2. **Leader transfer**: the caller must become the group's leader without changing what a parent waits on. Either the caller
   adopts the leader's pid (the `Process` is keyed by pid and `THREAD_PID_MAP`, `THREAD_IDENTITY`'s tgid slot, the child
   channel and `CHILD_CHANNELS` all name it), or the leader's `Process`, channel and wait identity are re-pointed at the caller
   while the leader's task terminates without publishing an exit.
3. Then the existing leader path (`with_target`, `install`, `enter`) runs unchanged on the caller, and the new program keeps the
   pid a parent is waiting on.

The hard parts are the identity caches (122 `.tgid` reads, `current_thread_tgid_process`) and ensuring no sibling is ever
left running on the old space (the `DRAIN INCOMPLETE` backstop in `thread::drain`). Needs a gate probe (`execthread` is the
start) and `scripts/gate.py` on both architectures; the AArch64 kernel's exec path has the same `stop_siblings` shape and
should be checked at the same time.
