# Rump at SMP=4: first run, and what rump currently does

**Date:** 2026-08-18
**Status:** First multi-core rump measurement ever taken. The rump devbox
**works at SMP=4 with the 2026-08-18 scheduler fix** (1 ms tick + wake-deadline
preemption) and **degrades badly without it** (BKL storm, bulk download never
completes). Single run per arm, one confound (below) — treat as a strong
signal, not a settled result. A second half, added the same day, answers
whether the **fiber backend should be replaced by real threads** or its
context switch folded into `akuma-exec` — code analysis, not a rebuild.
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

## Would threads beat fibers? Would folding the switch into `akuma-exec` help?

Asked after the SMP=4 result above, since both premises the fiber backend was
chosen under have since changed. Answers from the code, not from a rebuild —
nothing here was measured today.

### The context switch is not the cost

`akfiber_switch` (`rumpuser/src/fiber.rs:503-520`, seeded by `akctx_make` at
`:184`) saves 22 callee-saved registers and swaps SP, entirely in userspace:
no syscall, no exception entry, no TTBR/ASID change. That is tens of
nanoseconds against a **~20 ms** proxied round-trip — call it 0.0001 % of the
cost.

So **folding the switch into `akuma-exec` would make rump slower, not
faster.** Every switch would become a syscall plus exception entry plus a
kernel scheduling decision, and it would put rump's cooperative semantics
(fibers that assume they run until they yield) on a preemptive scheduler that
does not share them. The kernel's useful contribution to this path is **wake
delivery**, not switching — which is what the 2026-08-18 tick + wake-preemption
change already improved, and what the `poll()`-on-the-channel-fd design
(`poll_fd_waiters`, `:450`) is built to exploit.

### `ncpu = 1` is the structural blocker for threads

`rumpuser_getparam` returns `_RUMPUSER_NCPU = "1"`
(`rumpuser/src/lib.rs:377`). The NetBSD kernel inside `rump_server` therefore
has **one virtual CPU**, and its own scheduler hands that vCPU to one lwp at a
time. Nineteen OS threads contending for one virtual CPU is exactly the futex
storm that was measured when the pthread backend was retired
([`FIBER_HANDOFF.md`](FIBER_HANDOFF.md)): `clone`/`futex` **20 / 2606** vs
**0 / 0**, `curl` **62.8 s → 16.3 s**, OS threads **19 → 1**.

So reverting to pthreads *by itself* re-buys the storm. Two things have
changed since that measurement, and it is worth being precise about which:

- **Changed:** the scheduler-latency component. A woken thread now gets the
  CPU *next* at ~1 ms, instead of waiting a ~35 ms round-robin pass. Three
  fiber-only workarounds would also disappear — the `rump_server` park loop,
  the `sp_serve_fd.c` `pthread_cond_*` redirect (which exists *only* because
  one blocking OS thread deadlocks the fiber scheduler), and the receiver's
  `poll(0)` + yield loop (`FIBER_HANDOFF.md` items 1-3).
- **Unchanged:** the serialization. `ncpu = 1` means no amount of OS-thread
  parallelism reaches the NetBSD kernel.

### The configuration nobody has tried

Fibers and `ncpu > 1` are **mutually exclusive by construction** — one OS
thread cannot occupy four virtual CPUs. So "should we go back to threads" is
really *"do we want rump to use more than one core"*, and that question only
became askable once rump ran multi-core at all, which first happened in the
run documented above.

The untried configuration is:

    _RUMPUSER_NCPU=4  +  pthread backend  +  Akuma SMP=4  +  1 ms tick

Every one of those four preconditions is new or newly verified. The pthread
backend is still live behind `--no-default-features`
(`rumpuser/Cargo.toml`); `docker-build-rump-server.sh` rebuilds and relinks
the binary.

**Snag if you try it:** the comment at `rumpuser/src/lib.rs:365` says a set
`RUMP_NCPU` wins, but the code does `getenv(name)` on the *param* name, so the
variable it actually reads is `_RUMPUSER_NCPU`. NetBSD's own librumpuser maps
`RUMP_NCPU` → `_RUMPUSER_NCPU`; this port does not. **Setting `RUMP_NCPU=4`
today silently does nothing.**

### The tick change exposed the fiber scheduler's own quantization

The fiber scheduler's entire time base is **integer milliseconds**: `now()`
truncates `clock_gettime` to ms (`fiber.rs:387-392`), `poll()` timeouts are
ms, and the idle fallback is a ms-granularity `nanosleep` (`schedule()`,
`:623`). That rounding was invisible when a 1 ms sleep cost 35 ms — it was
noise on top of a far larger floor. Now that a 1 ms sleep costs 1.01 ms, the
fiber scheduler's own quantization is a **co-dominant term**, not a rounding
error. Sub-ms rump latency needs `now()` in µs regardless of which threading
backend wins.

### An exact analogue of the fix that worked

`fiber.rs:355` defines `const WAKE_LOCALITY_HINT: bool = false` — a run-next
hint for the fiber a wakeup targeted, with the machinery present in
`schedule()` (the `hint` branch) and **compiled out**. That is the same
mechanism, under nearly the same name and in the same disabled state, as the
kernel's `WAKEUP_LOCALITY_HINT`, whose fix on 2026-08-18 was to arm it from
the one path that never set it. Whether the fiber version has the same
untapped win is unknown; it is a one-const experiment.

Related and already disproved, so nobody re-tries it: `EAGER_FD_POLL`
(`fiber.rs:380`, also `false`) polls fd-waiters on every scheduling decision.
Measured 2026-07-19 — `poll` calls 1892 → 20124, idle CPU ~1 % → 13-26 %,
best-case latency 288 → 216 ms, **median unchanged**. The in-code comment says
to rate-limit or debounce it if ever revisited.

### Recommendation

Do not drop rump on today's evidence, and do not build anything speculative —
run the one experiment. If rump cannot beat its own single-vCPU fiber numbers
with four virtual CPUs, four real cores, and a scheduler that wakes in 1 ms,
then "rump on 1 vCPU is a fool's errand" becomes a measured conclusion with a
receipt rather than a hunch, and the support can be removed on that basis.
Roughly an hour of work; either outcome settles a question that has been open
since July.

## Not done

- **HTTPS A/B across the two ticks.** A `https` probe (curl timing breakdown:
  DNS / connect / first byte / total against `https://example.com/`) is wired
  into `scripts/sched_audit_matrix.py` but was never run on either arm. It is
  one `--only https` invocation per arm and is the cheapest remaining answer
  about interactive rump behaviour under the new tick.
- **A 10 ms SMP=1 rump arm** — the third point that would separate "the tick"
  from "rump has never seen more than one core".
- **The `_RUMPUSER_NCPU=4` + pthread-backend experiment** described above —
  the decide-or-drop measurement.
- **`WAKE_LOCALITY_HINT` in the fiber scheduler** — one const, directly
  analogous to the kernel fix that worked.
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
- [`FIBER_HANDOFF.md`](FIBER_HANDOFF.md) — why the pthread backend was retired
  (`curl` 62.8 → 16.3 s, threads 19 → 1, `clone`/`futex` 20/2606 → 0/0) and the
  three fiber-only workarounds it required.
- [`RUMP_PLUS_HERD.md`](RUMP_PLUS_HERD.md), [`RUMP_SYSPROXY.md`](RUMP_SYSPROXY.md),
  [`PHASE01_BUILDRUMP.md`](PHASE01_BUILDRUMP.md), [`PHASE2_RUMPUSER.md`](PHASE2_RUMPUSER.md).
- [`../../acceptance/11_netbsd_rumpkernel_irc.md`](../../acceptance/11_netbsd_rumpkernel_irc.md).
