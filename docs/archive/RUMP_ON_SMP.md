# Rump at SMP=4: first run, and what rump currently does

**Date:** 2026-08-18
**Status:** First multi-core rump measurement ever taken. The rump devbox
**works at SMP=4 with the 2026-08-18 scheduler fix** (1 ms tick + wake-deadline
preemption) and **degrades badly without it** (BKL storm, bulk download never
completes). Single run per arm, one confound (below) — treat as a strong
signal, not a settled result.
**Written because:** the scheduling audit
([`SCHEDULING_INVESTIGATION.md`](SCHEDULING_INVESTIGATION.md)) shipped a
profile-gated tick with rump as its named open risk, and the tick's owner
asked whether rump survives it on real SMP.

## Why nobody had run this before

`scripts/build_devbox.sh` builds `--no-default-features`, which drops
`smp-shared` — the feature that brings up secondary cores. Every rump run in
the archive is therefore single-core, and `overlays/devbox/run.sh` even
`unset SMP`s (that line is about the removed multikernel, but the effect
stands). A real SMP=4 rump kernel needs `smp-shared` added to
`DEVBOX_FEATURES`:

```bash
cargo build --release --no-default-features --features \
  "devbox,sound,no-tests,rump-tests,smp-shared,sc-aio,sc-sysv-ipc,\
sc-framebuffer,sc-containers,sc-timerfd,sc-eventfd,sc-pidfd,sc-epoll"
```

That combination — NetBSD sysproxy stack **and** shared-kernel SMP — had never
been built, let alone booted. So a failure in it is not automatically a
statement about the tick.

## What was measured

Both arms: `devbox.img`, `-smp 4 -m 4096`, `RUMP_NIC=1` (NIC1 → `/dev/net/tap0`,
host :2223 → box :22), same host, same session, driven by
`scripts/sched_audit_matrix.py` (`rump-smp4-fixed` / `rump-smp4-base`).
Medians, round 1 discarded.

| metric | base (10 ms tick, no preempt) | fixed (1 ms + preempt) |
|---|---|---|
| boots to sshd over the rump stack | yes | yes |
| secondary cores online | 4/4 | 4/4 |
| rump self-tests at boot | pass | pass |
| `sleepbench` 1 ms actual | 3.88 ms | **1.01 ms** |
| `pollbench` 1 ms actual | 3.69 ms | **1.01 ms** |
| `pipebench` | 1.83 µs/iter | 2.22 µs/iter |
| 128 MB download over rump | **never completed** (0/5 rounds parsed) | 104.4 s (5/5, spread 98–105 s) |
| `[BKL] stuck` lines | **5225**, every one `tag=511` | **0** |
| panics / OOM / rump errors | 0 / 0 / 0 | 0 / 0 / 0 |

### The headline

The concern that motivated the profile gate was that a 1 ms tick might upset
rump's timing. The measurement says the opposite: **the 10 ms arm is the one
that falls over.** Under bulk network load at SMP=4 it enters a sustained
`tag=511` BKL storm and the download never finishes, while the 1 ms +
preemption arm runs it to completion five times with zero stuck lines.

Consistent with prior work, though not proven here: `tag=511` storms are known
to be load-driven ([`SMP_SHARED_ONCPU_GATE.md`](SMP_SHARED_ONCPU_GATE.md),
[`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md)), and the sysproxy
round-trip was already root-caused to **Akuma-side scheduler latency** rather
than rump compute — the 2026-07-19 fix cut keystroke p50 318 → 219 ms by
coalescing each request/reply into a single `pipe_write` so one wake carries a
complete frame (`archive/RUMP_SYSPROXY_LATENCY_FIX.md` Phase 3q). A scheduler
that hands the woken party the CPU immediately is exactly what that path
wants; a 10 ms round-robin wait per leg is exactly what it does not.

### Confounds, stated plainly

- **The base arm ran on battery, the fixed arm on AC.** Host throttling is a
  documented trap in this investigation and it moves throughput numbers ~40 %.
  It does not plausibly manufacture 5225 BKL-stuck lines against 0, but the
  base arm's latency cells should not be quoted as precise.
- **One run per arm**, no interleaving.
- `smp-shared` + rump is a new build combination; the storm could be a
  property of *that* rather than of the tick. Distinguishing them needs a
  10 ms arm at SMP=1 (rump's historical configuration) for a third point.

## What rump currently does, as of 2026-08-18

Condensed from [`../reference/subsystems/rump-stack.md`](../reference/subsystems/rump-stack.md)
(stability grade **C**) — read that for internals; this is the "does it work"
view.

**Works:**

- **Box 0 on rump by default** (`rump-default`, part of the `devbox` feature):
  the kernel spawns `/bin/rump_server` at boot, handshakes sysproxy over fd 3,
  and every ordinary process — login shell, sshd, curl — has its `AF_INET`
  syscalls transparently forwarded. No `LD_PRELOAD`, no per-binary linking.
- **DHCP + real internet** over `/dev/net/tap0` (NIC1), including HTTP and
  HTTPS via `bootstrap/bin/curl`.
- **sshd over the rump stack**, reachable on host :2223.
- **herd-owned `stack = rump` boxes** (a rump box that is not box 0, on an
  otherwise smoltcp build), with `join_box` sharing.
- **The fiber backend** (`threads_fiber`, default): rump's ~19 pthread
  kthreads collapsed onto one OS thread with a hand-rolled aarch64 context
  switch. This is what made rump usable on one vCPU at all.
- **SMP=4** — as of today, with the scheduler fix. New.

**Known costs (all pre-existing, none introduced by the tick):**

- **~8.7× slower than native smoltcp on an HTTP GET** (~1.13 s vs ~0.13 s),
  ~6× on HTTPS, measured 2026-07-19.
- **~20 ms per proxied round-trip floor**, rump_server's own fiber cadence.
- **Bulk throughput is the worst axis**: 128 MB in 104 s (~1.25 MB/s) versus
  2.5 s (~50 MB/s) on the smoltcp devbox at the same SMP=4. Every `recv` is a
  proxied syscall, so throughput is round-trip-bound by construction. This has
  never been tuned — the sysproxy work optimised *interactive* latency.
- **No auto-restart** for herd rump boxes (`restart = false`).
- Rump is **deferred** as a project (2026-07-19); devbox-smoltcp is the
  recommended image.

For balance: `akuma-net`/smoltcp is not a tuned stack either, and carries
suspected allocation issues on its hot paths. The rump-vs-smoltcp ratios above
are "two unoptimised stacks compared", not "rump against a good baseline".

## Not done

- **HTTPS A/B across the two ticks.** A `https` probe (curl timing breakdown:
  DNS / connect / first byte / total against `https://example.com/`) is wired
  into `scripts/sched_audit_matrix.py` but was never run on either arm. It is
  one `--only https` invocation per arm and is the cheapest remaining answer
  about interactive rump behaviour under the new tick.
- **A 10 ms SMP=1 rump arm** — the third point that would separate "the tick"
  from "rump has never seen more than one core".
- **Re-run of the base arm on AC.**
- Root-causing the `tag=511` storm itself.

## Background

- [`SCHEDULING_INVESTIGATION.md`](SCHEDULING_INVESTIGATION.md) — the tick +
  wake-preemption fix these arms A/B, and the full scheduler matrix.
- [`SCHEDULING_AUDIT.md`](SCHEDULING_AUDIT.md) — the code-level audit behind it.
- [`RUMP_SYSPROXY_LATENCY_FIX.md`](RUMP_SYSPROXY_LATENCY_FIX.md) — why the
  sysproxy path is scheduler-latency-bound.
- [`RUMP_LATENCY_SLEEP_FIX.md`](RUMP_LATENCY_SLEEP_FIX.md) — the disproved
  heartbeat theory ("don't re-try this").
- [`RUMP_PLUS_HERD.md`](RUMP_PLUS_HERD.md), [`RUMP_SYSPROXY.md`](RUMP_SYSPROXY.md),
  [`PHASE01_BUILDRUMP.md`](PHASE01_BUILDRUMP.md), [`PHASE2_RUMPUSER.md`](PHASE2_RUMPUSER.md).
- [`../../acceptance/11_netbsd_rumpkernel_irc.md`](../../acceptance/11_netbsd_rumpkernel_irc.md).
