# amd64: a core's timer stopped for good, and `hget` hung on it unkillable

**Date:** 2026-10-13/14. **Where:** ryzen metal, entry 14 (`smp=8`, wifi).
**Trigger:** `box pull` failed with `manifest fetch failed: IoError`; chasing it
found `hget https://example.com/` hanging after 1–4 runs on a fresh boot, a
process that read `R`, accumulated CPU time at 100 %, and ignored `kill -9`.

## What it looked like, and what it was not

- `curl` and `aria2c` fetched the same hosts fine, so not the network path;
  the same TLS code against the same Docker hosts on a Mac was fine, so not
  `embedded-tls` or the cipher (the user's first suspicion) or the RNG
  (`RDRAND`, output checked).
- Stage markers put the hang inside `connect()`. `/proc/net/tcp` showed the
  socket `CLOSE_WAIT` — handshake done, server idle-closed it — and the kernel
  `wait_until`'s 10 s cap never fired; a one-shot print for "wait outlived twice
  its timeout" never printed either, so the thread was not looping in the wait.
- `/proc/<pid>/stat` said `utime` +100/s and `stime` 0, but a per-pid tick
  sampler never saw that pid current. The "CPU time" was the scheduler's
  wall-clock bookkeeping for a thread it believed was running.
- **Per-core tick counters, dumped from the BSP every 1000 ticks, found it: core
  1 frozen at 11509 while the other seven advanced ~1000 per dump.** A thread
  halted on a core whose timer would never fire again.

## Root cause

`lapic::arm_deadline` (2026-10-09, for sub-tick futex wakeups) switches the
core's LVT timer to **one-shot** before `hlt`, and `restore_periodic` puts
periodic mode back *after* the halt returns. That ordering assumes the halting
task resumes right after the interrupt. In `idle_loop` it need not: the timer
interrupt that ends the `hlt` can preempt and switch away from inside the
handler, and the idle task, which owns the `restore_periodic` call, is not
resumed until much later. Meanwhile the core runs other threads on a timer that
has fired once and will not again.

The next thread to `allow_tick` (the wifi link's `net_blocking_relax`, i.e. any
blocking `connect`/`recv`) calls `arm_deadline`, which compares its computed
count with `REG_TIMER_CUR`. The expired one-shot reads 0, so "the periodic tick
serves it sooner" and it returns without arming — and the thread executes `hlt`
on a core with no timer. Nothing wakes it: not the wait's deadline, not a
signal (delivered at a tick or syscall return), not `kill -9`. `ONESHOT[cpu]`
stays set, so the flag that would have restored the mode is only ever read by
the thread that can no longer run.

## Fix

- `lapic::on_tick` calls `restore_periodic()` after the EOI. A tick is the
  one-shot firing; this is the one place guaranteed to run.
- `lapic::arm_deadline` calls `restore_periodic()` first, so a stale one-shot
  left by any other route cannot masquerade as "the periodic tick is near".

Verified on ryzen: 40/40 `hget` runs complete (the unfixed kernel hung within
1–4 on four separate boots), all eight cores' tick counters advance, `box pull
alpine:3.20` completes, and `box run` / `box run -i` (pty) execute the pulled
Alpine image. No host test: the defect is in LAPIC register state across a
context switch.

## A second, smaller defect found on the way

`finish_connect_wait` named `Established | Closed | Closing | TimeWait` as the
states that end a blocking connect, so a server that completed the handshake
and closed before the waiter looked (`CloseWait`, or further along) satisfied
nothing, and a connect to a connection that *had* come up returned `ETIMEDOUT`
after 10 s. `connect_completed` now names every post-handshake state; the wait
and `connect_outcome` use it, with host tests.

## Still open

- `box grab: no processes found in box` seen five times on containers started
  right after `box close`; not reproduced in a clean script. `box grab` now
  prints what it saw (id, recorded primary, live/in-box slots, the primary's
  slot) so the next one explains itself.
- `kill -9` of a thread halted with no timer is still ignored; the fix removes
  the way it got there, not the inability to interrupt a `hlt` with no wake
  source.
- `/proc/<pid>/stat` utime for a descheduled-but-"running" thread is wall time.

## Background

- `AKUMA_AMD64_SIGKILL_NATIVE_PATH.md` — why a cross-core terminate is wrong on
  this kernel; this hang is the same "R, unkillable" shape with a different cause.
- `AKUMA_AMD64_BOXES_AUDIT.md` — the box work this was found during.
