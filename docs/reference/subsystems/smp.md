# SMP / multikernel (one kernel per core)

Source: `src/smp.rs` (4,173 lines) + the host-testable `crates/akuma-smp` (the
pure ring/descriptor/state-machine half). Behind `cfg(kernel_smp)` (the `smp`
feature), paired with the `release-smp` profile — the default build compiles
none of this. For the base (single-core) scheduler this extends, see
[`scheduler.md`](scheduler.md). For the GIC-level doorbell mechanics, see
[`drivers/gic.md`](drivers/gic.md) "Multikernel doorbell". For smoltcp being
BSP-only, see [`networking.md`](networking.md). For herd's `core` service
field, see [`containers.md`](containers.md).

> **Stability: C (active risk).** All 28 commits to `src/smp.rs` land in a
> five-day window, 2026-06-28 to 2026-07-01 — barely two weeks dormant as of
> this writing, nowhere near the 2+ month bar for "stable." It ships its own
> in-tree self-test suite (R2/R3a/R3b/R4a) rather than host unit tests, and is
> feature-gated off by default. The recurring lesson: **a cooperative
> (never-preempted) idle thread swallows an involuntary scheduler SGI** —
> waking a parked forwarder on a reply requires a *voluntary* reschedule, or
> the wake is silently deferred to the next timer tick (see "Known bugs").

This is **not** shared-memory SMP. Each core runs its own private instance of
the kernel — own PMM, own heap, own thread pool, own scheduler, own page
tables — coordinated only through one small shared region. "Multikernel," not
"multiprocessor."

## Boot / activation lifecycle

Each PE moves `STATE_OFFLINE → STATE_BOOTING → STATE_ONLINE → STATE_PARKED`
(`crates/akuma-smp/src/descriptor.rs:24-33`), then either activates or falls
back to `STATE_OFFLINE` on watchdog timeout:

1. **Probe + partition.** `probe_dtb` (`smp.rs:835`) parses `/cpus` once,
   before the heap is up (a large-RAM heap can otherwise clobber the DTB),
   indexing each PE's MPIDR by `aff0 = mpidr & 0xff` (`collect_mpidrs`,
   `smp.rs:312`). `bringup_secondaries` (`smp.rs:892`) then carves RAM into
   per-core slices (`akuma_smp::partition`, 2 MiB-aligned, last core absorbs
   the remainder), builds each secondary's isolated page table (see "Per-core
   isolation"), and PSCI `CPU_ON`s it (`PSCI_CPU_ON = 0xC400_0003`) with the
   shared `MachineConfig`'s physical address as `context_id`.
2. **Self-test, then park.** A secondary runs the R2/R3a/R3b/R4a soundness
   suite (private PMM/heap round-trip, cooperative then preemptive per-core
   scheduler, cross-core forward transport), then
   `secondary_park_and_await_init` (`smp.rs:2168`) announces `STATE_PARKED`,
   brings up its doorbell SGI + a periodic virtual-timer tick, and
   `WFI`-sleeps draining its inbox until `MSG_CORE_INIT` arrives or a **120 s
   watchdog** (`CORE_INIT_WATCHDOG_US = 120_000_000`, `smp.rs:66`) expires —
   confirmed against source, matching the figure in `scheduler.md`.
3. **Activate or shut down.** `MSG_CORE_INIT` → `secondary_steady_state`
   (`smp.rs:2252`) stands up the real scheduler (preemptible idle thread,
   ~10 ms tick). No message within the watchdog → `secondary_shutdown`
   (`smp.rs:2233`) marks `STATE_OFFLINE` and PSCI `CPU_OFF`s; a later
   `core_init` re-`CPU_ON`s it, re-running the whole sequence.
4. **`core_init` syscall** (`smp.rs:3861`), the only source of `MSG_CORE_INIT`,
   is restricted to `cfg.initiator` (hardcoded to the BSP today — a field so a
   later elected leader can take over without a format change), publishes the
   init-program path into `cfg.init_program[idx]` **before** pushing the
   message (ring ordering guarantees the secondary sees the path first), then
   rings the target's doorbell; idempotent against an already-online core.
   herd is the intended caller (see "Config knobs").

There is deliberately **no cross-core spawn message**: the initiator names a
path, and the target core's own kernel fetches the ELF and spawns it locally.

## The `akuma-smp` message bus

The **only** cross-core data path is a lock-free MPSC ring per core (`Ring`,
`crates/akuma-smp/src/ring.rs`) living in `MachineConfig`'s shared page(s).
Many peers `push` (CAS on `tail`); only the owner `pop`s (`head`). A full ring
**drops** rather than blocking, so a wedged consumer can't stall a producer.
`RING_CAP = 8` — low-rate control traffic, not a data plane.

| Message | Value | Payload | Purpose |
|---|---|---|---|
| `MSG_PRESSURE` | 1 | none | "Under memory pressure — debtors, repay." |
| `MSG_REPAID` | 2 | `v0`=base, `v1`=len | Repayment addressed to a creditor core. |
| `MSG_FWD_ECHO_REQ`/`REPLY` | 3 / 4 | len, nonce | R4a transport self-test round-trip. |
| `MSG_FWD_SYSCALL_REQ`/`REPLY` | 5 / 6 | (`fwd_call`/`fwd_bounce`) / retval+nr | Generic forwarded syscall. |
| `MSG_CORE_INIT` | 7 | none | "Activate — leave PARKED, run your role." |

Side channels off the control ring keep high-volume or ordering-sensitive
data separate: `console_rings` (async stdout/stderr, see below),
`fwd_bounce`/`fwd_call` (forward argument/pointer payload), `fwd_reply` (a
**dedicated per-core mailbox**, not the inbox, for the return value — so the
idle loop's inbox drain can never swallow it), and `init_program` (the path a
parked core spawns on activation). `heartbeat` is a shared per-core liveness
counter; a stalled one means that core is gone. `enforcement_results` is
pinned to byte offset 0 by contract — an asm fault handler writes it via
`TPIDR_EL1 + idx*4`, and a host test asserts the offset.

The doorbell SGI (`trigger_sgi_core`, GIC detail in
[`drivers/gic.md`](drivers/gic.md)) is the **wake**, not the transport: a
producer pushes to the ring, then rings the target's doorbell so a `WFI`-ed
peer observes the message promptly instead of on its own next timer tick.

**Console (async, not forwarded):** a secondary's restricted table doesn't map
the UART, so `console::print` can't run there. Output goes through one
`emit()` chokepoint into the core's own `ConsoleRing` (`console_emit`,
`smp.rs:142`); a BSP drainer thread (`start_console_drainer`, `smp.rs:189`)
polls every ring and writes the UART. Deliberately not a forwarded syscall —
`capability_of` excludes console — fire-and-forget batching suits tty output,
whereas forwarding would round-trip every character.

## Per-core isolation: what's private vs. shared

`build_isolated_table` (`smp.rs:733`) builds each secondary's restricted page
table from a bump allocator scoped to that core's own partition — never the
BSP's `pmm`:

- **Shared, read-only:** kernel `.text`/`.rodata`, same identity VA everywhere.
- **Replicated, private:** the kernel's `.data`/`.bss` mapped RW to a
  **private physical page per core at the same VA**
  (`replicate_writable_window`, `smp.rs:684`; seeded from a pristine snapshot
  for `.data`, zeroed for `.bss`). This is what makes `static PMM`, the heap
  allocator (TALC), the process table, and the thread pool **per-core
  instances of the same static** — same code, isolated by page tables.
- **Shared, read-write:** exactly `MachineConfig`'s page(s) — the message bus.
  Everything else in a peer's `.data`/`.bss` is unmapped, so a stray
  cross-core dereference **faults**. `run_enforcement_test` (`smp.rs:1620`)
  probes this and records `ENF_FAULTED` (good) vs. `ENF_LEAKED` per core.
- **Private, identity-mapped:** the core's boot stack + `PerCpu` page, its
  whole partition as 2 MiB RW blocks (so its own PMM can hand out any
  in-partition page), and its own GIC redistributor frames.

Net effect: PMM, heap, and thread pool are fully duplicated per core; only the
message-bus page(s) are truly global.

### Syscalls needing core-0-only resources

Core 0 is the **capability owner** for VFS and networking (`capability_of` /
`capability_owner`, `smp.rs:2385-2408` — hardcoded "Phase-0", a future
descriptor field per the design doc). `Local` syscalls (threads, memory,
futexes, time, signals, `getpid`, …) always resolve against a core's own
replicated state; a `Vfs`/`Net` syscall elsewhere forwards
(`forward_syscall`, `smp.rs:2451`; serviced by `service_forwarded_syscall`,
`smp.rs:3471`): the caller takes a per-core cooperative lock
(`fwd_slot_acquire` — one outstanding forward per core, the slot is
single-buffered), copies pointer-argument bytes into `fwd_bounce`, sets
`fwd_call` (nr + scalar args), and pushes `MSG_FWD_SYSCALL_REQ` to core 0's
inbox. It then parks (`FWD_AWAITING_REPLY` set), `yield_now()`-ing on its
**dedicated** `fwd_reply` mailbox, bounded by a 5 s transport timeout distinct
from logical blocking (a `recv()` with no data yet returns `-EAGAIN`, retried
one layer up by the `fwd_*` helpers). Core 0's persistent forward-server
thread (`start_fwd_server`, `smp.rs:3821`) runs the **real** syscall, copies
outbound bytes back, publishes the return value to the requester's mailbox,
and rings its doorbell; the requester's doorbell handler
(`secondary_doorbell_handler`, `smp.rs:2135`) sees `FWD_AWAITING_REPLY` and
forces a **voluntary** reschedule so the waiter resumes in tens of µs instead
of on the next tick.

"Exec is recursive forwarding": a pinned process has no local VFS, so
spawning it means `openat`/`read`*/`close` forwarded to core 0, one
`FWD_BOUNCE_CAP` (64 KiB) chunk per round trip (`fetch_file_forwarded`,
`smp.rs:2501`), then a normal local spawn. `spawn_init_program`
(`smp.rs:2729`) runs this once per core once core 0's forward-server is
ready, for the path herd wrote into `init_program[idx]` — never a cross-core
process injection. See [`containers.md`](containers.md) for herd's `core`
field. **Local-NIC exception:** under `rump`, `RUMP_NIC_CORE = 2`
(`smp.rs:731`) maps the virtio-mmio page directly and runs its own NetBSD
rump stack (`secondary_init_local_nic`, `smp.rs:2666`) instead of forwarding
sockets — needs `SMP>=3` and QEMU `CORE2_NIC=1`; see
[`networking.md`](networking.md) for the two-stack model.

## Config knobs

SMP-specific; see [`config-flags.md`](config-flags.md) for the general system.

| Knob | Default | Effect |
|---|---|---|
| `smp` (feature) / `release-smp` (profile) | off | Compiles this subsystem in. No wrapper script — invoke directly: `cargo build --profile release-smp --features smp`. |
| `SMP` (env, `cargo_runner.sh`) | `1` | QEMU `-smp N` vCPU count. |
| `CORE2_NIC` (env) | `0` | `1` adds a third virtio-net for `RUMP_NIC_CORE`'s local stack. |
| `MULTIKERNEL_INIT_HERD` (`config.rs:616`) | `true` | Secondaries stay `PARKED`; herd calls `core_init` via `/proc/cores`. |
| `AUTO_START_HERD` (`config.rs:604`) | `true` | Must also be true for herd-managed activation; else the BSP auto-activates every secondary at bringup with no init program. |
| `RUN_FWD_BENCH` (`smp.rs:2545`, const) | `false` | In-kernel forward-latency/bulk-transfer self-test; needs `SMP>=3`. |

## Known constraints and invariants

- **Box + non-BSP core pin are mutually exclusive**, enforced in **herd**
  userspace (`is_boxed`, `userspace/herd/src/main.rs:853`, checked in
  `start_service`), not the kernel — a boxed service pinned to `core != 0` is
  rejected outright. Box bookkeeping (`RUMP_BOXES`, box VFS roots) is
  per-kernel-private state core 0 owns as part of its VFS/net capability; a
  secondary's replicated `.data`/`.bss` gives it its own *empty* copy, not a
  view onto core 0's. herd also rejects a second service pinned to an
  already-claimed core (`core_init` overwrites the pending slot,
  `userspace/herd/src/main.rs:1025`). See [`containers.md`](containers.md).
- **`MAX_CORES = 8`** (`crates/akuma-smp/src/descriptor.rs:19`), comfortably
  under the aff0<16 limit `trigger_sgi_core` enforces for QEMU `virt`'s single
  affinity-1 cluster (see `gic.md`) — `smp.rs` never separately guards that
  limit because its own cap is stricter.
- **Debt-based reclaim (`MSG_PRESSURE`/`MSG_REPAID`) is simulator-validated
  but still faked** — no real page moves between partitions yet
  (`akuma_smp::CoreStateMachine` is host-unit-tested; the kernel just drives
  it over real rings).

## Known bugs (historical)

**Cooperative idle thread swallowed the wake.** Before the doorbell-reschedule
fix, forwarded-syscall reply latency was bound by the per-core timer tick
(~136 ms at a 43.7 ms tick, ≈3 ticks) because the cooperative idle thread
can't be involuntarily preempted before its timeout — ringing only an
involuntary scheduler SGI on reply made no difference. Fix: the owner rings
the requester's doorbell on reply, and the doorbell handler calls
`request_voluntary_reschedule()` before self-ringing its scheduler SGI, which
bypasses the cooperative-idle guard unconditionally: ~136,000 µs → ~45 µs.
See `archive/MULTIKERNEL_NETWORKING_EXPERIMENT.md` §2-4.

All 28 commits to `src/smp.rs` (2026-06-28 to 2026-07-01) took the subsystem
from "second core spins" (M0) through isolation (R2), cooperative/preemptive
per-core scheduling (R3a/R3b), the forward transport (R4a), a pinned EL0
process (R4b), herd core-awareness, and a local rump stack on a secondary —
see `git log --oneline -- src/smp.rs`.

## Background

- `archive/MULTIKERNEL.md` — the full design doc and milestone log (M0-R4b,
  §1-§15): the primary source for the "why" behind per-core replication, the
  debt-reclaim protocol, and the forwarding model.
- `archive/MULTIKERNEL_NETWORKING_EXPERIMENT.md` — the forwarded-syscall
  latency investigation and the case for a NIC-local rump stack.
- [`acceptance/archive/12_multikernel_demo.md`](../../../acceptance/archive/12_multikernel_demo.md)
  — the end-to-end demo (pinned `curl`, then interactive `sshd` on a secondary).
  Archived (2026-08-10) with the rest of the acceptance suite trim, but still the
  only playbook covering the experimental `smp`/multikernel feature.
- `userspace/herd/docs/CORE_AWARE_SCHEDULING.md` — herd's side of core pinning
  and the box/core mutual-exclusion rule.
