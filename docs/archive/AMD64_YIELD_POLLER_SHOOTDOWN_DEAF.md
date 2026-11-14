# amd64: a kernel thread that polls by yielding never acked a TLB shootdown (2026-11-12)

**Status: fixed in the tree (`crates/akuma-kernel-amd64/src/sched.rs` `yield_now`), verified on QEMU TCG
under host load. Not a metal finding; nothing seen on the metal is attributed to it.**

## Symptom

Routing amd64's `spawn` (301) through the shared spawn (`akuma-exec`, via glue's
`spawn_request` — `AKUMA_MULTIARCH_ATTEMPT_0.md`) made the `SMP=2` boot suite fail
2 boots in 4 when the host was busy (12 `yes` hogs on a 12-core Mac), and now and then
without load:

```
  [TLB] stuck: 1 peer(s) unacked, generation=2 sender=1 missing=0x0000000000000001
  fork: `sh -c "uname; echo DONE"` exited 0   [FAIL] got 0xffffffffffffffff want 0x0
  fork: teardown leaks nothing   [FAIL] ...
```

The failing test moved between runs (`execve`, `fork`, `cwd`, `redirect`), always one
that drives `/bin/sh` through `run_sh_capture`, and `0xffff…` is that loop running out
its 4M-iteration budget. HEAD (amd64's own 301) ran the same load 4/4 clean.

## Reading it

A temporary line in the stuck report (now permanent: `smp::peer_snapshot`) printed the
silent core's state: `core 0 task=0 idle=0 halted=0`. Task 0 is the boot thread, which
runs the suite, and it was not halted. Two QEMU monitor captures of core 0 while it was
stuck (`-monitor tcp:…`, `info registers -a`) put it in `akuma_threading::try_yield` and
in `akuma_exec::process::children::get_child_channel`, both with `RFL` showing `IF=0`.
The core was running, not wedged: it was the test's poll loop (`sys_read` on the child's
`ChildStdout`, `sys_waitpid`, `sched::yield_now`).

## Cause

Ring 0 on amd64 runs with `IF` clear. A kernel thread receives the shootdown IPI only
when its core enables interrupts (idle `hlt`, or ring 3), or when it services its own
mailbox. The only places that serviced the mailbox were the BKL ticket wait
(`bkl_spin_assist`), `wait_for_acks` itself, and the poll loops that call
`masked_wait_assist`. A thread that polls by **yielding while it holds the BKL** reaches
none of those. It never waits on the ticket, and if nothing else is runnable on its core,
it never goes idle either.

The sender was the child shell's `fork` on core 1. Fork's share pass broadcasts without
the BKL (`AMD64_TLB_STUCK_AS_LOCK.md`), so the poller holding the lock did not stop it.
`wait_for_acks` then spun until the poller's core happened to open `IF`.

Why the shared spawn exposed it: the child is now a thread made by
`spawn_user_thread_fn_for_process` and placed by the scheduler, so `sh` starts on the
core the poller is *not* on. Its address space is then live on both cores, and its fork
shootdown targets the poller's core. The gap itself is older than this change: any kernel
thread that polls by yielding had it.

## Fix

`sched::yield_now` calls `shootdown::masked_wait_assist()` first. That is the IPI landing
at this instruction, which is what `IF=1` there would have done. Every yield is already a
point where the thread gives up the CPU, so nothing can rely on not being flushed there.
With nothing pending, it costs one atomic load.

## Verified

- `SMP=2` boot suite under 12 hogs, before: 2 of 4 boots failed, each with `[TLB] stuck`.
  After: 4 of 4 clean, no `[TLB] stuck` (833/0 three times, 829/0 once). The 829 run
  recorded four fewer checks, which matches `spawn_test` skipping its four stdio checks
  for an already-exited child. Its log was overwritten before I could confirm that.

## Still open

- Any IRQ-masked kernel loop that neither yields nor calls `masked_wait_assist` has the
  same gap. The rule in `docs/reference/subsystems/memory.md` names it.
