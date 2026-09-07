# amd64: `netpoll laps 0` was a stalled clock retry, not a starved daemon

**Date:** 2026-09-07
**Scope:** the one open failure left by C1 step 3 batch 2
(`docs/archive/AKUMA_SELF_HOSTING_AMD64.md`) — `net: the netpoll daemon is
being scheduled [FAIL]` on the bare-metal box, with `netpoll laps 0` after
410534 boot-thread yields across the full 2 s budget.
**Status:** root-caused and fixed. Verified on three rigs, including the one
that had **never** passed this check.

## What it was believed to be

> "During `boot::self_tests`, on real hardware, the daemon doesn't get
> picked… it's boot-thread-relative — QEMU/TCG gives 101 laps in 101 yields,
> the daemon running on every one. Cornering it means instrumenting the picker
> across several reboot cycles."

Every part of that is a reasonable reading of the evidence available, and the
conclusion — a scheduler question — was wrong. The daemon was picked. It ran.
It was inside one lap for longer than the whole budget.

## The one number that could not say so

`net::netpoll_spawn_selftest` measured `NETPOLL_LAPS`, which the daemon bumps
**last** in its loop:

```rust
drain_step();
crate::clock::sync_tick();
mem_watch_tick();
let laps = NETPOLL_LAPS.fetch_add(1, Relaxed) + 1;   // <- the only counter
```

So `laps == 0` is consistent with four completely different machines:

1. the task was never given the CPU (a picker question),
2. it was given the CPU and is inside `drain_step` (a NIC question),
3. it is past the drain and inside `clock::sync_tick` (a clock question),
4. it is inside `mem_watch_tick`.

Only the first is a scheduler fault, and it is the one the check's *name*
asserted. A single counter at the end of a loop cannot distinguish "the loop
never started" from "the loop is slow", and this loop had a step in it that
can legitimately take seconds.

Three counters (`NETPOLL_ENTERED` / `NETPOLL_DRAINED` / `NETPOLL_TICKED`),
bumped at three points, settle it in one boot. They cost three relaxed
increments on a loop that already does a device poll, and they stay in the
shipped kernel: this failure only appears on real hardware, where adding
instrumentation costs a reboot cycle.

## What it actually was

The instrumented boot, on the OVMF/GRUB rig:

```
  net: netpoll laps 0
  net: netpoll yields waited 1154875
  net: netpoll daemon entered 1        <- it WAS scheduled
  net: netpoll drains completed 1      <- it polled the network once
  net: netpoll ticks completed 0       <- and never got past sync_tick
  net: netpoll daemon thread state 2   <- RUNNING
  net: netpoll daemon on-cpu gate 1    <- on another core, right now
```

The daemon reached lap one, finished its drain, entered
`clock::sync_tick()` — and stayed there.

`sync_tick` retries SNTP while the wall clock is unset, rate-limited to one
attempt per `RETRY_INTERVAL_US` (15 s). The rate limit was armed **inside
`sync_tick`**, so `sync_via_sntp` — the boot one-shot, which runs a few lines
earlier in `boot::self_tests` — did not participate in it. On a machine whose
boot attempt *fails*, `NEXT_RETRY_US` was therefore still `0` when the daemon
started, and the daemon's **very first lap** re-attempted the thing that had
just failed microseconds before, for the whole `RETRY_TIMEOUT_US` budget of
2.5 s.

The self-test gives the daemon 2.0 s.

**So the check could not pass on any machine whose boot SNTP failed.** It was
not flaky; it was deterministic on a condition nobody was looking at.

The serial log had been saying so all along, in the right order:

```
519:  net: the netpoll drain reaches quiescence   [OK]
524:  clock: boot: could not resolve pool.ntp.org via 10.0.2.3   <- the trigger
525:  net:  netpoll daemon spawned in slot 4
529:  net: the netpoll daemon is being scheduled   [FAIL]
546:  FAILED: net: the netpoll daemon is being scheduled
550:  clock: retry: could not resolve pool.ntp.org via 10.0.2.3  <- lap 1 returning
```

Line 550 is the daemon's first lap finishing — **after** the verdict at 546.

### And the second witness nobody read

`[rtl] STALL #1 after 2000000 idle laps` printed inside the same window. That
is the Realtek glue counting two million `Device::receive` calls that returned
nothing — i.e. *something* was hammering the NIC while the lap counter stood
still. It was `dns::resolve_a`'s own poll loop, inside the stall. Read
together with `laps 0` it rules out "the daemon is not running" outright; read
alone, next to a `[FAIL]` about scheduling, it looked like unrelated NIC noise.

## Why every rig agreed and still disagreed

| rig | boot SNTP | result |
|---|---|---|
| QEMU/TCG, PVH | succeeds (slirp DNS) | `sync_tick` is a no-op — 101 laps in 101 yields, always green |
| OVMF/GRUB q35, multiboot2 | **cannot** succeed — the e1000 is undriven | 2.5 s stall — never passed, blamed on the NIC |
| HP box, bare metal, multiboot2 | *sometimes* | coin flip, blamed on the picker |

The correlation looks like "multiboot2 vs PVH" and looks like "hardware vs
VMM", and it is neither: it is "did the boot-time SNTP land before the netpoll
test". The bare-metal boot verified below has it **failing** during the suite
and succeeding forty lines later, which is precisely the flake.

## The fix

**The rate limit belongs to the attempt, not to `sync_tick`.**
`report_outcome` arms `NEXT_RETRY_US` on any non-`Ok` outcome, so a failed
boot one-shot pushes the next attempt 15 s out whichever path made it. The
daemon's early laps are then what the doc always claimed they were — a relaxed
load and a compare.

The retry itself is unchanged, and still runs inside the daemon: that is a
deliberate design (`netpoll_daemon`'s own doc — one fewer thing the scheduler
must keep alive, and it runs exactly when the network is being driven). What
changes is that it can no longer fire on lap one of a daemon that was spawned
seconds after an identical failed attempt.

### The check, split in two

One line answered four questions. Now:

```
net: the netpoll daemon is being scheduled   <- NETPOLL_ENTERED >= 1  (the picker)
net: the netpoll daemon completes laps       <- laps > 100            (throughput)
```

The first is what the name always claimed and `laps` could never answer. A
future failure of the second, with the first green, is a slow lap and the
three notes below it say which half.

## A duplicate daemon, found on the way

`multiboot2.rs`'s `boot_to_init` called `spawn_netpoll()` unconditionally after
the suite had already spawned one, so **every full-suite bare-metal boot ran two
netpoll daemons** for the life of the machine, each calling
`smoltcp_net::poll()` on its own core. That is exactly the concurrent
kernel-side stack access `settle_for_dhcp`'s doc records as having deadlocked
on a spinlock the poll step takes (`AKUMA_FIRECRACKER_AMD64.md` §3.30),
arranged permanently rather than for one window.

The PVH path never had it — `kmain` relies on the suite's spawn — which is why
it only ever existed on the metal. `spawn_netpoll` is idempotent now (it keeps
the slot and asks `x86_slot_is_live`, so a daemon that somehow died is still
replaced), and `boot_to_init` still calls it because the `skiptests` path shares
that function and has no suite to have spawned one.

It was masked by the very failure above: the second daemon starting right after
the suite is what made the box "start lapping the moment the suite ends", which
read as evidence the daemon was healthy and merely unscheduled.

## Verification

| rig | entry | before | after |
|---|---|---|---|
| local QEMU/TCG `SMP=4` | PVH | 440 / 0 | **441 / 0**, laps 101 / 101 yields |
| OVMF/GRUB q35 `SMP=4` | multiboot2 | 430 / **1** | **432 / 0**, laps 101 / 101 yields |
| HP box, bare metal `SMP=4` | multiboot2 | 430 / **1**, laps 0 / 410534 yields | **432 / 0**, laps 101 / 101 yields |

The OVMF rig is the strong result: its NIC is undrivable, so DNS can never
work there and `sync_tick` will fail for the life of the boot. It passes
anyway, which is the property that was missing.

Host tests 143 suites / 1359 checks, 0 failures. amd64 clippy clean.

**AArch64 proven unchanged**, and the method matters:
`crates/akuma-threading` is shared, and the added `x86_slot_debug` is behind
`#[cfg(target_arch = "x86_64")]`. A naive A/B — delete the block, rebuild,
compare sections — reports `.text` and `.rodata` **different**, which looks
like the cfg leaking. It is not: removing 19 lines shifts every panic
location below them, and those line numbers live in `.rodata` with `.text`
referencing them. Replacing the block with 19 comment lines instead makes both
sections **byte-identical**. When A/B-ing a shared crate this way, **pad the
removed block to the same line count**, or the panic-location table alone will
tell you the kernel changed.

## Background

- `docs/archive/AKUMA_AMD64_C1_STEP3_PREREQUISITES.md` — §4's "pre-existing
  hang", which is this bug's *other* half: the same 2.5 s stall, on a rig where
  the deadline could not fire either, presented as an unbounded hang. Fixing
  the bound (its own timer bracket, then a stalled-clock backstop) is what
  turned the hang into the measurable `[FAIL]` this doc closes.
- `docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.30 — why the SNTP fetch has to
  happen in the window between the drain and the daemon, and the spinlock the
  duplicate daemon was standing on.
- `docs/runbooks/amd64-bare-metal-loop.md` — the OVMF rig that reproduced this
  without a reboot, and the rule that a boot-order change gets a run on it.
