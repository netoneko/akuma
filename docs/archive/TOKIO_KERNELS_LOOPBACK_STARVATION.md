# Tokio kernels: a blocking `accept` starved every other thread (2026-11-19)

## Symptom
`scripts/gate.py`'s `tcp_loopback` check (accept on one thread, `connect` on another, 64 KiB echo) never completed on
Taoxichuan, Shanghai and Denver; the guest made no progress. It passed on the main amd64 kernel. `tcp_bind`, UDP loopback and
a single-threaded connect-then-accept all passed, which is what made it look like a loopback or pump bug.

## Cause
Not the loopback ring and not the pump. The thread blocked in `accept` ran `akuma_net::socket::wait_until`, whose idle hooks
on the tokio kernels (`akuma-tokio-kernel-core/src/net.rs`: `blocking_relax`, `park_until`, `yield_now`) pumped the NIC and
halted (`wait_lap`) without ever offering the core to the scheduler. Ring-0 code is not preempted on these kernels (the tick
switches only a thread it interrupted in ring 3), so the sibling thread, parked in `nanosleep` before its `connect`, was never
readied: its deadline passed but only the wake pass inside `try_yield` readies a sleeper. The main amd64 kernel's
`net_blocking_relax` already had the rule ("a sleeper whose deadline has passed needs the scheduler, not a halt").
`strace`-style evidence: the console stopped at `server accepting` and the client's `STEP client connecting` (after a 300 ms
sleep) never printed.

## Fix
`NetMachine::can_park()` (default `false`) and a shared `lap()`: a wait lap that can park calls
`schedule_blocking(now + 1 ms)` instead of halting. Taoxichuan's amd64 machine answers `can_park` with the tick's own
preemption gate (a ring task with a slot, after init's thread is published). The AArch64 platform kernels keep the old
behaviour (default `false`) until someone can boot them.

## Verified
`tcpsteps` (a stepwise probe) and the gate's `tcp_loopback` on Taoxichuan, Shanghai and Denver: pass.

## Also found, and not a bug
`many_threads` "hanging" was `clone` answering `EAGAIN` past the 48-slot spawn ring (`NSLOTS`), after which std panics and
the process aborts; the other checks then looked wedged. Slots come back (`spawn_churn`, `thread_churn`). The gate's
`many_threads` is now 32 and `threads_128` records the capacity limit (an xfail).

## Still open
* AArch64 platform kernels (`can_park`).
* The main amd64 kernel's `tcp_loopback` timed out once after the spawn checks and passed on later runs (listed `flaky:`).
* The 48-slot ring: tokio's blocking pool can ask for 512 threads.

## Background
`docs/reference/subsystems/networking.md` § "Stacks, loopback, namespaces and the threads that drive them",
`docs/archive/MERGE_GATE_FIRST_RUN.md`.
