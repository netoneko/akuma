# `getppid` returned the forking thread's pid: every command codex ran died with SIGTERM (2026-11-17)

## Symptom

`codex` (0.169.0) on the ryzen box: every shell command failed. In the TUI it first looked like the code-mode host
dying, then — once codex was pointed at a herd-run host — `exited 143 in 0ms` with no output, for *any* command, `true`
included. 143 is 128 + SIGTERM. The kernel's `dmesg` showed only `[signal] pid=N killed by signal 15 (default action)`:
the victim, no sender.

## Cause

`Process::parent_pid` is the pid of the **forking thread**, on purpose: `wait4`, orphan reaping and
`kill_child_processes` all key on it (`akuma-exec::process`, `fork_process`). `sys_getppid` returned it as it was.
Linux's `getppid` is the parent's **thread-group id**. For a child forked from the main thread the two are equal; for a
child forked from any other thread they are not.

codex spawns each command from a tokio worker thread and, in the child, sets `PR_SET_PDEATHSIG(SIGTERM)` and then checks
`getppid() != <parent's getpid()>` to catch a parent that died in between. The check failed on every spawn, so the child
`raise(SIGTERM)`'d itself before `exec`, and codex then killed the process group.

Found by the kernel's own syscall trace in a KVM guest (`strace_err strace_nr=62,200,234`): `pid=150 nr=200 (tkill)
a1=<its own tid> a2=0xf` followed by `pid=86 nr=62 (kill) a1=-150 a2=9`. A Python probe then reproduced it with no codex:
`fork()` from a worker thread, child's `getppid()` != parent's `getpid()` (main-thread fork: equal).

## Fix

`sys_getppid` (`crates/akuma-syscalls-glue/src/proc.rs`) resolves `parent_pid` to that process's `tgid` (falling back to
the stored value if the parent is gone), and leaves `parent_pid` itself alone. Both kernels share this arm.

Not changed, and still reporting the thread pid: the `ppid` field of `/proc/<pid>/stat` and `/proc/<pid>/status` (they
render `parent_pid` directly). A tool that compares those with `getpid()` from a worker-thread fork would see the same
mismatch.

## Verified

A headless kernel built from the fix, booted as a KVM guest on the same image: codex's `exec_command` of
`ls /tmp | head -3` returned `exit_code 0` with the listing; the unfixed kernel returned 143 with none. Host unit tests
and clippy ran in the pre-commit hook.

## Still open

- The `/proc/<pid>/{stat,status}` ppid above.
- No regression test: the host-test seam does not reach a fork-from-a-thread. A forktest probe
  (`userspace/forktest/`) that forks from a worker thread and compares `getppid()` is the natural gate.
- Two other codex-on-Akuma problems that were separate from this one: the host spawned over `grpc+stdio://` dies on first
  use, and `bwrap` cannot run (`/proc/sys/kernel/overflowuid`, `/proc/self/ns`). Procedure and workarounds:
  [`../runbooks/debug-codex-on-akuma.md`](../runbooks/debug-codex-on-akuma.md).

## Background

[`CLOCK_GETTIME_EINTR_ON_SIGNAL.md`](CLOCK_GETTIME_EINTR_ON_SIGNAL.md) is the earlier codex-on-Akuma incident.
