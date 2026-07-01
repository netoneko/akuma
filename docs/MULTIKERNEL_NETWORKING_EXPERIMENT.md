# Multikernel Networking Experiment

Cross-core syscall forwarding under the one-kernel-per-core multikernel (see `MULTIKERNEL.md`,
branch `smp-attempt-0`). A process pinned to a secondary core has no local VFS or network stack,
so every VFS/socket syscall is *forwarded* to the BSP (core 0), which owns those capabilities.
This doc records why forwarded networking was "extremely slowly," the fixes, the measurements,
and the plan to run a NIC-local network stack (rump) on a secondary instead of forwarding.

All numbers below: QEMU `virt`, HVF, `SMP=3 MEMORY=2048`, virtual timer 24 MHz
(`TIMER_INTERVAL_TICKS = 0x10_0000` ⇒ one tick ≈ 43.7 ms). Reproduce with the in-kernel
self-test (`RUN_FWD_BENCH` in `src/smp.rs`, off by default — needs `SMP>=3` so its bench core
doesn't collide with a herd-pinned service on core 1).

## 1. Symptom

`curl` and `sshd` pinned to a secondary core worked but were extremely slow (commit
`c93e2d7`: "sshd + curl works on multikernel, albeit extremely slowly").

## 2. Root cause — reply latency bounded by the timer tick

One forwarded syscall is a round-trip: the secondary publishes the request to the BSP's inbox
and parks; the BSP's forward-server thread services it and publishes the reply to the
secondary's dedicated mailbox; the secondary observes the reply and returns.

The reply was published promptly. The cost was that the **secondary didn't get scheduled to
observe it**:

- The parked forwarder yields (`yield_now`) to the per-core **idle boot thread**, which `WFI`s.
- The idle boot thread is marked **cooperative** (`crates/akuma-exec/src/threading/mod.rs`,
  `IDLE_THREAD_IDX`). `schedule_indices` refuses to *involuntarily* preempt a cooperative
  RUNNING thread until its 100 ms timeout — so only a timer tick eventually switched back to
  the forwarder, and empirically that took ~3 ticks.
- There was **no doorbell** rung on the reply, so nothing woke the secondary sooner.

Measured per-round-trip: **~136 ms** (≈ 3.1 × the 43.7 ms tick).

## 3. The fix — doorbell wake + voluntary reschedule

Two parts, both required (`src/smp.rs`):

1. **BSP rings the requester's doorbell** the instant it publishes a reply
   (`service_fwd_requests` → `trigger_sgi_core(from, DOORBELL_SGI)`).
2. **The secondary's doorbell handler forces a voluntary reschedule** when a forward is
   outstanding (`FWD_AWAITING_REPLY`): it calls the new
   `akuma_exec::threading::request_voluntary_reschedule()` (sets `VOLUNTARY_SCHEDULE`) then
   rings its own scheduler SGI. A *voluntary* switch bypasses the cooperative-idle guard and
   preempts idle→waiter at once.

An earlier attempt that rang only an *involuntary* scheduler SGI made **no difference** (the
cooperative idle thread ignored it) — that dead end is why part (2) marks the switch voluntary.

## 4. Results

### Per-syscall latency (payload-free `clock_gettime`, 40 round-trips)

| reschedule | µs / round-trip |
|---|---|
| OFF (old, timer-tick bound) | ~136,000 |
| ON (doorbell wake) | **~45** |

≈ **3000× faster**. Verified identical on an idle dedicated core and on a core also running
sshd — so the win is the reschedule, not reduced contention.

### Bulk transfer — forwarded fetch of `/bin/curl` (1,511,904 B), reschedule ON

| `FWD_BOUNCE_CAP` | round-trips | time | throughput |
|---|---|---|---|
| 16 KiB | 93 | 20.2 ms | 74 MB/s |
| **64 KiB** | 24 | 12.6 ms | **119 MB/s** |
| 128 KiB | 12 | 13.4 ms | 113 MB/s |

The doorbell fix alone took this fetch from an extrapolated ~12.6 s (93 × 136 ms) to 20 ms at
16 KiB. Raising the bounce to 64 KiB adds 1.6× more, but 128 KiB is **worse** — we're
**copy-bound** past ~64 KiB (the byte-wise `AtomicU8` bounce copy dominates), and 128 KiB also
overflowed a 256 KiB thread stack via the `[u8; FWD_BOUNCE_CAP]` staging arrays. So **64 KiB is
the knee** and the value kept. The real bulk lever past here is a shared `(offset,len)` arena
that skips the per-chunk copy (`MULTIKERNEL.md` §16), not a bigger control-path buffer.

### End-to-end: `curl -sS https://ifconfig.me` on core 1 (all networking forwarded to core 0)

Real workload — forwarded DNS (UDP), the HTTPS TCP connection + TLS handshake to
`34.160.111.145:443`, and the HTTP GET; curl's own ELF (1.5 MB) is fetched over forwarded
open/read first. Both runs returned the public IP `87.71.63.90`.

| reschedule | spawn → result | speedup |
|---|---|---|
| OFF (old) | ~8.5 s | — |
| ON (fix) | **~1.6 s** | ~5.3× |

The end-to-end speedup is "only" ~5× (not 3000×) because curl's wall-time is dominated by real
internet DNS + TLS RTT, which is fixed regardless of forwarding speed; the 3000× shows in the
syscall-bound phases. (The ELF fetch stays fast even before the fix, because it runs on the boot
thread, which busy-spins — there's no idle thread to hand off to until the process is spawned.)

## 5. Related changes

- **herd: reject two services pinned to the same core.** A kernel runs one init program per
  core (`core_init` overwrites the pending program), so pinning both `sshd` and `netcheck` to
  core 1 silently clobbered one. herd now tracks pinned cores and logs an error rather than
  clobbering (`userspace/herd/src/main.rs`, `HerdState::pinned_cores`).
- **`THREADING_HEARTBEAT_INTERVAL`** was raised — the BSP idle loop's `[Thread0] loop=` serial
  spam was throttling the guest ~10× under HVF, which distorted timing.

## 6. Reproduction / gotchas

- Build: `cargo build --profile release-smp --features smp,no-tests`. The `no-tests` is because
  an **unrelated pre-existing** boot self-test (`test_mmap_file_oom`, `src/process_tests.rs`)
  panics on the SMP boot — worth a separate look; it is not related to forwarding.
- Set `RUN_FWD_BENCH = true` (with `SMP>=3`) to run the in-kernel forwarding self-test, which
  PASSes iff mean round-trip < `FWD_LATENCY_MAX_US` (5 ms), catching a doorbell-wake regression.
- The `curl` measurement enabled the `netcheck` herd service (`curl -sS https://ifconfig.me`,
  `core = 1`) on the disk and temporarily disabled `sshd` (both pin core 1). Needs
  `/etc/ssl/certs/ca-certificates.crt` staged for HTTPS.
- Grep the serial log with `grep -a` (it carries control chars).

## 7. Next: rump networking on a secondary (Stage 0/1)

Forwarding is now fast, but a secondary still has no *local* network stack. The experiment: run
the NetBSD rump TCP/IP stack on a secondary so its networking is local instead of forwarded.
Blocker + plan are in the memory notes and `MULTIKERNEL.md`: secondaries map **no device MMIO**
(`build_isolated_table` maps only the core's own GIC redistributor), so a secondary can't touch
a NIC as-is. Stage 0/1: give a dedicated NIC (a 3rd `virtio-net` on `virtio-mmio-bus.5`) to the
secondary — map that one virtio-mmio page into its isolated table, register `akuma_net`'s
runtime on that core, bind `rump_tap` to that slot — then run `rumphttp` (self-contained static
binary) pinned there and compare its HTTP-GET latency to the forwarded `curl` above.
