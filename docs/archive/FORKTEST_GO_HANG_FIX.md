# Forktest_parent Go Hang — ROOT-CAUSED & FIXED (2026-07-22)

## Problem Statement (as originally filed)

The Go-based `forktest_parent` stress test (from `userspace/forktest/`) hung under
SMP=2 and SMP=4: it printed `Launching child 0...` and never completed, leaving
multiple `forktest_parent` PIDs in `ps` and no kernel crash diagnostics.

## Resolution summary

**Not an SMP bug at all.** The hang reproduced identically at SMP=1 (the original
investigation never ran the single-core baseline). The extra PIDs in `ps` were the
Go runtime's OS threads (M's), not leaked forks — no fork had ever happened.

**Root cause:** the kernel's `waitid` blocked on processes that are NOT children
of the caller. Go's `os/exec` (Go ≥1.23) probes pidfd support once per process
(`os.checkPidfd`) before its first fork:

1. `pidfd_open(getpid())` — open a pidfd of ITSELF (Akuma implements this; it
   succeeded because `forktest_parent` is itself in `CHILD_CHANNELS` as a child
   of the login shell).
2. `waitid(P_PIDFD, fd, WEXITED)` — on Linux this returns **ECHILD immediately**
   (a process is not its own child). Akuma's `sys_waitid` P_PIDFD arm resolved
   the pidfd to PID N and blocked on PID N's exit channel with **no parentage
   check** — i.e. the process blocked forever waiting for *its own* exit.

So the main goroutine hung inside `exec.Cmd.Start()` before `clone` was ever
issued. `[futex-dbg]` + syscall tracing showed the last userspace action was
`[pidfd] open pid=5 → fd=8`, then silence — no clone, no error pipe.

**Why it regressed:** pidfd support (`src/syscall/pidfd.rs`) landed 2026-06-19
(git-in-VM work). Before that, `pidfd_open` returned ENOSYS, Go's probe failed,
and os/exec used the classic fork+wait4 path (which worked since April).

## The fix

`wait*` on a process that is not your child now fails with ECHILD instead of
blocking, matching Linux:

- `crates/akuma-exec/src/process/children.rs::is_child_of_group(child_pid,
  waiter_tgid)` — parentage check against the caller's **thread group** (the
  registered parent is whichever thread called fork/clone; a multithreaded Go
  parent may wait from a different M, so raw pid equality would break real
  waits). Falls back to `find_process` to resolve a non-leader thread's tgid.
- `src/syscall/proc.rs::sys_waitid` — P_PID and P_PIDFD arms return ECHILD when
  `!is_child_of_group(target, waiter_tgid)`.
- `src/syscall/proc.rs::sys_wait4` — explicit-pid arm gets the same guard (same
  latent self/non-child block).

With waitid returning ECHILD, Go's probe proceeds to `pidfd_send_signal`, which
Akuma does NOT implement (ENOSYS) → the probe cleanly reports "pidfd unsupported"
→ os/exec falls back to fork + wait4, the long-verified path.

Host tests: `is_child_of_group_matches_registered_parent_only`,
`is_child_of_group_unregistered_pid_is_not_a_child` (children.rs). 125 akuma-exec
host tests pass; clippy clean (smp-shared + default).

## Verified (2026-07-22, devbox-smoltcp, release-smp-shared)

- SMP=1: `forktest_parent -num_children=1 -duration=1s` completes, EXIT=0.
- SMP=2: basic AND `-num_children=3 -duration=15s -combined_stress` complete,
  EXIT=0; children handle SIGTERM gracefully; 0 RECOVERED / 0 WATCHDOG / 0 crashes.
- SMP=4: basic completes, EXIT=0.

## Remaining (separate bugs — see `SMP_GO_STRESS_CORRUPTION_FIX.md`)

SMP=4 + `-combined_stress` exposed two further kernel bugs, since evidence-mined
and given their own investigation prompt in
[`SMP_GO_STRESS_CORRUPTION_FIX.md`](SMP_GO_STRESS_CORRUPTION_FIX.md):

1. **Phantom-SVC misclassification** (present from SMP=2 up, silent): EL0
   demand-paging data aborts in Go's hottest loops (`memclrNoHeapPointers`'
   `dc zva`, `spanSet.push`'s `ldar`) sometimes classify as EC_SVC64. Prime
   suspect: `rust_sync_el0_handler_inner` reads `mrs esr_el1` *after* the BKL
   wrapper's preemptible spin window instead of an entry-time snapshot. The
   guard's give-up path then dispatches a garbage syscall nr, clobbering live
   `x0` → the observed `WILD-DA FAR=0x20000001a` and Go heap corruption.
2. **Hard BKL wedge at the SIGTERM/teardown deadline** (SMP=4): core 2 owns the
   BKL forever, 0 RECOVERED (not the ticket leak), 0 WATCHDOG. Possibly
   downstream of bug 1.

Repro:

```bash
SMP=4 overlays/devbox/run-smoltcp.sh   # then:
ssh -p 2222 root@localhost '/bin/forktest_parent -num_children=3 -duration=15s -combined_stress'
```

## Debugging tools that cracked it (keep in mind for next time)

- `src/config.rs::FUTEX_DBG_ENABLED = true` — futex WAIT/WOKE/WAKE trace with
  tids and timestamps.
- `src/config.rs::DEADLOCK_THREAD_DUMP_ENABLED = true` — periodic parked-thread
  resume-point dumps.
- PSTATS per-PID syscall profiles: the tell was `forktest_parent` having **no
  clone syscall at all** — the hang was before the fork, killing all
  futex/epoll/pipe hypotheses at once.
- Baseline first: running the SMP=1 case immediately reclassified the bug from
  "SMP scheduler race" to "syscall semantics".
