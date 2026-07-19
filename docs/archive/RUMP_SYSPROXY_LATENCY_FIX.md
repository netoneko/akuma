# Rump sysproxy latency — Phase 2 + 3 fix (scheduling, poll cadence, Nagle)

> **Status: LIVE (2026-07-18).** Phase 2a + 2b + 3a + 3b applied, measured,
> verified. Phase 3e/3f root-caused (not yet fixed) a residual ~300ms
> single-round-trip floor to fiber-scheduling contention, dominated by
> `rumpsp` (expected) and `rumpclk0`, the software-clock heartbeat (37% of
> all scheduling activity — unexpected). Phase 3g tried lowering `rumpclk0`'s
> rate and disproved it properly (worse, not better — see Phase 3g). Phase 3h
> ruled out CPU contention and the TTY/procfs forwarding path as causes (both
> measured, neither is it), fixing a real-but-not-load-bearing sshd
> inefficiency along the way. Phase 3i pinned the floor down precisely:
> `rumpsp` spends essentially 100% of a round trip cycling through **~6
> sequential wait/wake handoffs, each costing 30-95ms** — not a fixed floor,
> not idle time, not compute. Also tried and closed: removing
> `rumpvfs`/`rumpdev`/`rumpdev_bpf` to shed unused kthreads panics
> `socket()` itself (BSD ties sockets into the same fd/vnode machinery as
> files). Phase 3j named the handoffs precisely: **100% `sp_cond_wait`** —
> the sysproxy protocol's own producer/consumer worker-dispatch design
> (built for real OS-thread parallelism, pure overhead under one cooperative
> fiber), not kernel locks or the rump-CPU token. A wakeup-locality fix for
> it hung under contention (bug, disabled, never deployed) — moot anyway,
> since Phase 3k measured pure scheduling delay at only ~7ms/handoff average,
> confirming the real cost is genuine event-wait time on both sides of the
> handoff (worker AND the previously-invisible receiver wait), not
> scheduling slack. Phase 3l explained *why* the receiver's wait is slow
> despite being "event-driven": `schedule()` only reaches the poll branch
> that registers Akuma's fd-waker ~24% of the time (the rest, `rumpclk0` or
> another periodic kthread is already runnable and wins round-robin), and
> even when it does, the poll window itself is tick-sized (~14ms avg,
> whether resolved by a real event or a timeout) — both effects trace back
> to `rumpclk0`'s ubiquity (Phase 3f), not a bug in the waker. Phase 3m
> tried the obvious direct fix (poll fd-waiters eagerly on every scheduling
> decision, not just the idle fallback) — it worked exactly as designed
> (reach ratio flipped from 24%/76% to 62%/38%, best-case latency improved)
> but cost idle CPU going from ~1% to 13-26%, a bad trade for a fix that
> only moved the best case and left the median untouched. Reverted (toggle,
> code kept). **The floor is still open** — Phase 3p (measure the
> Akuma-kernel-side round trip in isolation, and profile the median-case
> path specifically) has the honest next step.
>
> See `docs/archive/RUMP_LATENCY_SLEEP_FIX.md` for the earlier **disproven**
> "lower `hz`" / `cv_broadcast`→`cv_signal` approach — disproven specifically
> on the **pthread** backend (a `cv_broadcast` thundering herd across ~19 real
> OS threads). That mechanism doesn't exist under the fiber backend (Phase 3e
> proved fiber context switches are cheap even under 50-way contention), so
> Phase 3f's `rumpclk0` finding was a **new data point, not a rerun of the
> old one** — though Phase 3g's fiber-specific hz test also came back
> negative, so the practical conclusion (don't lower hz) ends up the same on
> both backends, for different reasons.

## Symptom

Devbox SSH sessions (sshd on rump stack) were slow: `uname -a` over SSH took
~3.4 s, `echo hello` ~4.0 s. Interactive `busybox` input and `meow` screen
refresh felt laggy.

## Root cause

Two independent scheduling bottlenecks in the rump sysproxy path:

### 1. Network thread boost targeted the wrong thread (Phase 2a)

`start_default_stack` (`src/rump_proxy.rs`) spawns rump_server and immediately
discards the returned TID:

```rust
Ok((_tid, _chan, pid)) => pid,   // ← _tid thrown away
```

The scheduler's `NETWORK_THREAD_RATIO` boost (pre-empt to the network thread
every N ticks) was instead claimed by the proxy handshake kthread in
`attach_server` — a thread that parks in `loop { yield_now(); }` after the
one-time `Client::connect` handshake and does **zero** per-call sysproxy work.
Boosting a parked thread is a no-op.

The actual per-call work flows through rump_server's main OS thread (the fiber
scheduler). Without the boost, rump_server waited 4–9 scheduler quanta (10 ms
each) for a timeslice under load. Per-call sysproxy blocking was 84–104 ms.

### 2. Blocking-poll cadence too coarse for rump sockets (Phase 2b)

`sys_ppoll` / `sys_epoll_pwait` / `sys_pselect6` used a 10 ms re-poll floor
(`POLL_BLOCK_INTERVAL_US = 10_000`). When a process polls a rump socket fd,
10 ms of idle wakeups accumulate before the fd is re-checked. With many proxied
recvfrom/sendto calls per SSH session, this multiplied into hundreds of
milliseconds.

## Fix

### Phase 2a — register rump_server's TID (not the kthread's)

In `start_default_stack`, capture the TID returned by
`spawn_process_with_channel` and register it:

```rust
let (server_tid, pid) = match process::spawn_process_with_channel(...) {
    Ok((tid, _chan, pid)) => (tid, pid),
    ...
};
threading::set_network_thread_id(server_tid);
```

Removed the stale `set_network_thread_id(current_thread_id())` from the proxy
kthread in `attach_server` (it was behind `#[cfg(feature = "rump-default")]`
but was a no-op because the kthread parks after handshake).

Also: `run_async_main` in `src/main.rs:1161` already gates its own
`set_network_thread_id` behind `#[cfg(not(feature = "rump-default"))]` so the
two registrations don't conflict.

### Phase 2b — rump-aware 1 ms poll cadence

Added `RUMP_BLOCKING_POLL_INTERVAL_US = 1_000` in `src/syscall/poll.rs` and
`effective_poll_interval_us(has_rump_fd: bool)` which returns the shorter
interval when any polled fd is a rump socket. Helper functions:
`fd_is_rump_socket`, `any_fd_is_rump_socket`, `fd_set_contains_rump_socket`.

Three call sites updated:
- `sys_epoll_pwait` (poll.rs:743)
- `sys_pselect6` (poll.rs:847)
- `sys_ppoll` (poll.rs:932)

All gate on `#[cfg(feature = "rump")]` with `#[cfg(not(feature = "rump"))]`
stubs returning `false`.

## Measurement (2026-07-18, devbox, single kernel, QEMU virt, 4 GB)

| Metric | Baseline | Phase 2b only | Phase 2a + 2b | Phase 2a+2b+3a |
|--------|----------|---------------|----------------|----------------|
| `uname -a` over SSH (median) | ~3.4 s | ~2.7 s | 1.96 s (−42%) | **1.85 s** (−46%) |
| `echo hello` over SSH (median) | ~4.0 s | ~2.7 s | 1.97 s (−51%) | **1.81 s** (−55%) |

Per-call sysproxy trace (`RUMP_SP_TRACE=true`) before vs after:

| | Before (Phase 2b only) | After (Phase 2a + 2b) |
|---|---|---|
| `blk=` on sendto/recvfrom | 84–104 ms / 4–5 iterations | mostly **0 µs / 0 iterations** |
| Total per-call time | 84–104 ms | 50–110 ms (fiber round-trip) |

The `blk=0us/0` on most calls confirms rump_server now gets scheduled promptly
via the boost — the response arrives before the caller's deadline-based sleep
kicks in.

## Regression guards

`src/rump_tests.rs` — 5 tests, run at boot under `#[cfg(feature = "rump")]`:

- **T1**: `run_async_main`'s `set_network_thread_id` is gated by
  `#[cfg(not(feature = "rump-default"))]` (so it doesn't conflict with
  `start_default_stack`'s registration).
- **T2**: `start_default_stack` captures the TID from
  `spawn_process_with_channel` and calls `set_network_thread_id(server_tid)`.
  Also verifies the proxy kthread does NOT register itself.
- **T3/T4/T5**: `effective_poll_interval_us` returns correct values for
  non-rump (10 ms), rump (1 ms), and the ratio is meaningfully shorter.

Enabled on devbox via the `rump-tests` Cargo feature (allows rump_tests to
compile alongside `no-tests` without pulling in the full smoltcp-coupled test
suite).

## Phase 3a — tap fd was missing the tightened rump poll cadence

The original plan for Phase 3a was "add a readiness waker in `rumpcomp_tap.c`
so rump_server's fiber scheduler gets woken immediately on tap RX." That
premise doesn't hold: `/dev/net/tap0` (NIC1, `crates/akuma-net/src/rump_tap.rs`)
is **pure register-polled virtio, no RX IRQ** — nothing in the kernel
discovers a frame's arrival except a caller actively checking
`akuma_net::rump_tap::has_frame()`. There is no independent event to push a
wake from; a "waker" with nothing to fire it is a no-op. Building a real
push-wake would mean adding NIC1 GIC interrupt handling from scratch — out of
scope here (tracked as Phase 3a2 below, if ever needed).

What *was* real: `rumpcomp_tap.c`'s `rcvthread` already blocks on the tap fd
via `rumpuser_akuma_wait_fd` (fiber backend: `fiber.rs`'s `schedule()` idle
path calls the kernel's `poll()`/`sys_ppoll` on it; pthread backend: a direct
host `poll()`). That `sys_ppoll` blocking loop re-checks readiness on a fixed
cadence — 1 ms for "rump fds", 10 ms otherwise (`RUMP_BLOCKING_POLL_INTERVAL_US`
vs `BLOCKING_POLL_INTERVAL_US`, `src/syscall/poll.rs`). The classifier that
picks between them, `fd_is_rump_socket` (now `fd_wants_rump_poll_interval`),
only matched `FileDescriptor::RumpSocket` — **not** `FileDescriptor::Tap`.
So the RX kthread's per-frame wait — the highest-frequency rump-fd wait in the
whole system — was silently paying the 10 ms floor instead of the 1 ms one
Phase 2b introduced, on every SSH round-trip.

### Fix

`src/syscall/poll.rs`: renamed `fd_is_rump_socket` →
`fd_wants_rump_poll_interval` (and its `any_fd_*`/`fd_set_*` callers to
match, all private to this file) and widened its match to also accept
`FileDescriptor::Tap { .. }`. Three call sites already routed through it
(`sys_epoll_pwait`, `sys_pselect6`, `sys_ppoll`), so no new call sites were
needed — this was a pure classification fix.

## Phase 3b — Nagle + no `TCP_NODELAY` was strangling multi-write bursts

The remaining symptom after 3a: `busybox` per-keystroke echo and `meow`'s TUI
(3-pane layout, redrawn via a per-character cursor-position + glyph write —
confirmed by capturing its raw output: `\x1b[25;28He\x1b[25;29Hl…`) were still
"incredibly slow", and had never been slow under the smoltcp built-in stack.
That split (fine under smoltcp, bad under rump) plus "many small writes worse
than one big write of the same content" is the signature of Nagle's algorithm
interacting with delayed ACKs, not a scheduling problem.

Checked whether any box program could turn Nagle off: `libakuma::net` (what
`/bin/sshd` and `meow` are built against) has **no `set_nodelay` at all**, and
`proxy_setsockopt` (`src/rump_proxy.rs`) was a hardcoded no-op returning
success without forwarding anything to the real rump socket (originally added
so curl's best-effort `TCP_NODELAY`/keepalive calls wouldn't abort it — see
the function's existing doc comment). So every rump TCP socket ran with Nagle
on, permanently, with no way for a box program to opt out even if it tried.

### Measurement (interactive-latency harness, PTY over real SSH, devbox)

| Test | Before 3b | After 3b |
|---|---|---|
| Single keystroke echo (busybox, serialized round-trips) | ~320–370 ms median | ~320 ms median (unchanged — see below) |
| 50 separate `printf` writes in one command | 1427 ms | **458 ms** (−68%) |
| 200 separate `printf` writes in one command | 2070 ms | **518 ms** (−75%, barely above the 50-write case) |
| Same 50-line content as ONE write (`seq 1 50`) | 369.8 ms (already fine) | 284.5 ms |

Per-keystroke echo latency is unaffected by 3b because that test is fully
serialized (wait for the echo before sending the next key) — no unacked
backlog ever exists for Nagle to hold back, so its cost is the underlying
sysproxy/fiber round-trip floor (Phase 2's own "50–110 ms fiber round-trip",
compounded across the ≥2 rump-proxied hops in a keystroke→echo cycle), not
Nagle. The 50/200-write bursts *do* build up a backlog of small unacked
segments — that's exactly what Nagle+delayed-ACK penalizes, and exactly what
3b fixes. `meow`'s redraw is a burst-of-small-writes workload, so it benefits
the same way: post-fix, live per-keystroke TUI redraw in `meow` itself
measured ~140 ms median (captured via the same PTY harness, typing into a
running `meow` session).

### Fix

`src/rump_proxy.rs`: added `set_rump_sock_nodelay(proxy, rump_fd)` — forwards
a real NetBSD `setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, 1)` (syscall 105, via
the same `Op::Setsockopt` mapping that already existed but was unused; the
option value is served through `cin_override` the same way `connect`/`bind`
serve their translated `sockaddr_in`, so no real box pointer is needed).
Applied unconditionally — not opt-in — on every accepted rump TCP socket
(`proxy_accept`) and every outbound one (`proxy_connect`), since no box
program has a way to request it itself. `IPPROTO_TCP`/`TCP_NODELAY` are
numerically identical on Linux and NetBSD, so no translation table was
needed. `proxy_setsockopt` itself is untouched (still a no-op for whatever a
box program explicitly requests) — this is a proxy-internal default, not a
change to what setsockopt reports back to callers.

Tradeoff: Nagle exists to coalesce small writes for bulk-transfer efficiency;
disabling it unconditionally trades a small amount of packet-count efficiency
on large transfers (git clone, curl downloads) for interactive latency. Given
every fix in this doc already prioritizes interactive latency over bulk
throughput, and this is exactly what every SSH client/server and browser
defaults to, that tradeoff is the right one here.

## Also cleaned up: debug trace flags left on

`SYSCALL_DEBUG_NET_ENABLED` and `RUMP_SP_TRACE` (`src/config.rs`) had been
flipped to `true` while debugging Phase 2a/2b and never reverted — every
`ppoll`/`epoll_pwait`/rump-proxy call was logging a trace line, which was
flooding the serial console (10k+ lines for a single boot + a few SSH round
trips) and made the actual measurement logs hard to read. Both reverted to
their documented default (`false`).

## Phase 3e — the ~300 ms single-round-trip floor: root-caused, not fixed

Phase 3b closed the multi-write-burst gap but left single-round-trip cost
(one keystroke echo, ~300–370 ms) untouched. This investigated *where that
time actually goes*, live, inside `rump_server`.

### Instrumentation

`fiber.rs` (the cooperative rumpuser backend) had no tracing at all — the
existing `rumpuser_debug` feature only ever covered the old pthread backend
(`lib.rs`'s `tr!`/`trace()`). Added (gated behind `rumpuser_debug`, so zero
cost in the shipped binary):
- Per-call counters for `schedule()`'s idle-nanosleep path (`SCHED_IDLE_COUNT`/
  `SCHED_IDLE_MS_TOTAL`) and `wakeup_all`'s fan-out (`WAKE_ALL_CALLS`/
  `WAKE_ALL_WOKEN_TOTAL`) — cheap atomics, no I/O on the hot path.
- A snapshot trace on `wait()` returns that took ≥1 ms, using a single-`write()`
  line builder (`LineBuf`). The first cut logged unconditionally and per-digit
  — 36,240 `write()` calls in 60 s, ~98% of `rump_server`'s own CPU time spent
  tracing itself, which is *why* it's gated this tightly: over-instrumentation
  here directly perturbs the very thing being measured.

Rebuilt `rump_server` via `docker-build-rump-server.sh` with `rumpuser_debug`
added to the feature list, deployed via `scripts/populate_disk.sh --overlay`
(reversible — no base-image rebuild), booted devbox, ran one real keystroke
over SSH, read `/var/log/box/0/rump_server.log` (rump_server's `--log`
redirects its own stdout+stderr there, not to the kernel console).

### Finding

Across 4,461 sampled `wait()` calls: `idle_count=0` on **every single one**.
`schedule()`'s "nothing runnable, nanosleep" path was never entered once.
`p50=78 ms, p90=895 ms, max=61.9 s` per wait — under instrumentation load, so
inflated vs. the ~22–96 ms seen unadorned, but the binary signal (idle path
never taken) doesn't depend on that inflation. `wake_all_calls` climbed
steadily while `wake_all_woken_total` stayed at 2 the whole time — broadcasts
fire often but almost always find an empty waiter list.

**This rules out an idle-sleep/poll-cadence floor** (the mechanism Phase 2b/3a
fixed) as the cause here. The cost is round-robin **contention**: many fibers
are simultaneously runnable at all times, and a specific fiber's `wait()` →
`wake()` handoff has to wait its turn through the others.

### Isolating the scheduler mechanism itself (host-testable, no QEMU)

`fiber.rs` already had one `#[cfg(test)]` unit test
(`sp_mutex_condvar_ping_pong`, cross-built + run in a Docker arm64 container
via `test-fiber.sh` — no rump kernel, no QEMU, no disk image, seconds not
minutes). Added a second: `round_robin_contention_scales_with_fiber_count`
runs N independent cv_wait/cv_broadcast ping-pong *pairs* concurrently (real
fiber work, the same primitive NetBSD's own kthreads use) and times how long
one *tracked* pair takes to complete 200 rounds as N grows:

| fibers | time / round |
|---|---|
| 2  | 0.2 µs |
| 6  | 0.6 µs |
| 14 | 1.4 µs |
| 26 | 2.6 µs |
| 50 | 4.9 µs |

Clean, linear scaling, and **microseconds** even at 50 competing fibers — two
to three orders of magnitude below the millisecond-scale costs seen live.
**The fiber scheduler mechanism itself (context switches, round-robin,
wait/wake bookkeeping) is not the bottleneck.** Combined with the live
`idle_count=0` finding, this narrows the real cost to whatever *work* each
NetBSD kthread-as-fiber actually does once scheduled — real kernel-internal
processing (or possibly I/O), not Akuma-side scheduling overhead. That work
lives inside the vendored NetBSD `src-netbsd` tree and rump's own
single-virtual-CPU (`rump_schedule_cpu`) serialization, not in anything this
repo's Rust owns — profiling it further needs per-kthread identification
(names/purpose), which the current trace doesn't capture.

### Bug found and fixed along the way

Adding the second `#[test]` fn crashed the *first* one (SIGSEGV, exit 139).
Rust's std test harness runs every `#[test]` in a file in the same process
(separate OS threads, not separate processes); `THREAD_LIST`/`EXITED`/
`CURRENT` are process-wide statics, and `init_sched()` (called once per test)
allocates a fresh "self" `Thread` and never removes the previous one — so
whichever test ran second inherited a stale entry pointing at an OS
thread/stack that had already returned, and `schedule()`'s round-robin
eventually reached it. Fixed with a shared `reset_sched()` helper (clears
`THREAD_LIST`/`EXITED`/`CURRENT` before `init_sched()`) that both tests now
call, making them order-independent. This was a real latent bug in the
existing test suite, only ever exercised once a second scheduler test
existed.

### Also noted, not yet a live issue

`MAXFDWAIT = 8` (`fiber.rs`): `schedule()`'s idle path only polls the first 8
fd-blocked fibers it finds walking `THREAD_LIST`; a 9th concurrent fd-waiter
is silently excluded from the poll set with no warning, and would only ever
be woken by a timeout (or never, for an untimed wait). Today there are only
two real `wait_fd` users (`rumpcomp_tap.c`'s RX loop, `sp_serve_fd.c`'s
sysproxy reader), well under the cap — but it's a silent limit worth knowing
about before adding a third.

## Phase 3f — named the fibers: `rumpclk0` is 37% of all scheduling activity

Phase 3e proved the fiber scheduler *mechanism* isn't the cost but couldn't
say which fibers were dominating turns — the trace only had counts, no names.
Fix: `rumpuser_thread_create`'s `thrname` argument was silently discarded
(`_thrname`) even though every NetBSD kthread — and `rumpcomp_tap.c`'s
`tap-rx` — already passes one. Captured it into `Thread` (`fiber.rs`, gated
behind `rumpuser_debug`, dead weight otherwise) and added a per-name
switch-in histogram (`schedule_hist_bump`, called from `switch_threads`) —
pure in-memory counter updates on the hot path (a ≤32-slot linear scan,
no I/O), dumped rate-limited (every 200th slow `wait()`) piggybacked on the
existing trace point.

Deployed the same way as Phase 3e (rebuild via `docker-build-rump-server.sh`
with `rumpuser_debug` added, `scripts/populate_disk.sh --overlay`, boot,
one keystroke, read `/var/log/box/0/rump_server.log`), then summed the last
histogram snapshot (9,636 total switches captured over the run):

| fiber | switches | share |
|---|---|---|
| `rumpsp` (sysproxy server — the actual worker) | 4,352 | 45.2% |
| `rumpclk0` (rump's software clock / hardclock heartbeat) | 3,538 | **36.7%** |
| `rsi0/1` (softint, NIC0) | 825 | 8.6% |
| everything else (16 kthreads: `ipflow_slowtimo`, `tap-rx`, `vdrain`, `vrele`, `cachegc`, `ioflush`, `rt_timer`, `rsi0/2`, `rsi0/3`, `vmem_rehash`, `main`, …) | ~920 combined | ~9.5% |

`rumpsp` dominating is expected — it's the fiber doing our actual work.
**`rumpclk0` at 37% was not expected**, and together with `rumpsp` they're
~82% of all scheduling activity, with a long tail of NetBSD's other kthreads
each under 2%.

### Why this doesn't just re-confirm the disproven doc

`docs/archive/RUMP_LATENCY_SLEEP_FIX.md` already investigated "the 100 Hz
clock heartbeat is the cost" back on 2026-06-23 — lowering `hz` was built,
measured, and **disproven** (made curl latency *worse*, not better). But that
measurement predates the fiber backend entirely: it was diagnosing the
*pthread* backend's `cv_broadcast` thundering herd, where every clock tick's
CPU release woke ~19 real OS threads that all lost the race and went back to
sleep — a mechanism specific to pthreads. Phase 3e already proved fibers
don't have that problem (context switches are microseconds even at 50-way
contention). So `rumpclk0` showing up this large under fiber is a **different
mechanism** than what was ruled out in June — it's not a "thundering herd
disproof rerun," it's a fresh data point on a fresh backend. Worth testing on
its own merits, not skipped because of the old doc.

### Not yet done: actually testing a fix

This phase identified the culprit; it didn't fix it. Lowering `hz` (or
reducing `rumpclk0`'s tick rate some other way) is the obvious next
experiment, but it's a real behavior change to rump's internal timekeeping
(the old doc's own caveat still applies: coarser `hz` risks slower TCP
retransmit/DHCP timing, not just less overhead) and needs the same
measure-before-and-after discipline as every other phase here — not a blind
patch. Left for Phase 3g rather than rushed through in the same pass as the
identification.

## Phase 3g — tested lowering `rumpclk0`'s tick rate: disproven, for real this time

Set `rumpns_hz`/`rumpns_tick`/`rumpns_tickadj` (note the `rumpns_` prefix —
rump namespaces every kernel-internal symbol to avoid host-libc collisions;
plain `hz`/`tick`/`tickadj` link-fails with "undefined reference", confirmed
via `nm` on `librump.a`'s `param.o`) to `hz=20` in `rump_server.rs`, right
before `rump_init()` creates the `doclock` kthread. Booted clean — DHCP still
completed, SSH still worked — so this wasn't a repeat of the old build
failure, just a real behavior change measured properly:

| Test | hz=100 (baseline) | hz=20 |
|---|---|---|
| Single keystroke round trip | ~300–400ms | ~300–400ms (no change) |
| 50x printf burst | 458ms | **2675ms** (5.8x worse) |
| 200x printf burst | 518ms | **9742ms** (19x worse) |

Single-round-trip latency didn't improve at all, and burst workloads got
dramatically worse. Reverted immediately (`RUMP_HZ = 100`, i.e. a no-op —
left in the source as a documented, guarded experiment rather than deleted,
so a future attempt doesn't have to rediscover the `rumpns_` prefix gotcha).

Root cause of the regression: NetBSD's TCP/callout processing is
timer-driven. A burst needs many internal handoffs to actually get each
segment through the stack, and each of those is gated by the SAME clock
tick — coarsening it from 10ms to 50ms multiplies the wait per handoff
across all of them. This is a **different mechanism** than the old
pthread-era disproof (thundering herd), but it lands on the same
conclusion: don't lower `hz`. Two independent mechanisms, two backends, same
answer — this is now a settled question, not just an old warning to route
around.

## Phase 3h — ruled out CPU contention; found and fixed a real sshd inefficiency

Two things prompted by direct questions: "is `rump_server` clogging the
CPU?" and "have we ever actually looked at the TTY path?" — both answered
with real measurement rather than more rump-side speculation.

### CPU: not the bottleneck

Sampled host `ps -o %cpu` for the `qemu-system-aarch64` process (this VM is
`-smp 1`, so all guest CPU activity funnels through one host thread) during
the heaviest burst workload tested: **avg 1.14%, max 10.2%** across 227
samples. Cross-checked against Akuma's own in-VM per-process stats
(`[PSTATS]`): `rump_server`'s `in_kernel` time was ~99% of its wall-clock
life, but the breakdown showed why that's not compute — `ppoll` alone
accounted for 97%+ of that in-kernel time (115,410ms of 118,998ms over
120s). `ppoll` is a blocking-wait syscall; "in kernel" here means "inside
the syscall boundary," not "executing." `rump_server` is genuinely parked,
not busy. Confirms the Phase 3e/3f finding from a completely different
angle: this is a latency/scheduling-granularity problem, not a
CPU-contention one.

### TTY path: found a real inefficiency, measured it to not be the cause

`sshd`'s `bridge_process` (`userspace/sshd/src/protocol.rs`) forwarded every
single keystroke to the shell via `open("/proc/<pid>/fd/0", O_WRONLY)` →
`write_fd` → `close` — a full procfs path resolution **on every character**,
instead of opening the fd once per session and reusing it. Fixed: open once
before the bridge loop, write through the same fd for the session's
lifetime, close once at the end.

Measured with a controlled A/B (same devbox boot, swapping only the `sshd`
binary, 30-keystroke runs) rather than comparing against an earlier
measurement from a different point in the session — burst-workload timings
turned out to have high run-to-run variance (700ms–3000ms for the identical
test on the identical binary, most likely host-level vCPU scheduling jitter
from `-accel hvf` competing with other host load, not anything in this
repo's code) that would have made an uncontrolled before/after comparison
meaningless:

| | median | range |
|---|---|---|
| Original (open+write+close per keystroke) | 319.7ms | 288–460ms |
| Fixed (open once, reuse) | 319.7ms | 294–369ms |

**No measurable effect on latency** — identical median. The fix is real (2
fewer syscalls per keystroke, no functional change) and worth keeping, but
it conclusively rules out sshd's procfs-open pattern as a contributor to the
~300ms floor. The floor remains rooted in rump/fiber scheduling (Phase
3e/3f), not in the TTY-forwarding path or CPU contention.

## Phase 3i — the 300ms is a chain of ~6-10 short waits, not one floor

Extended the Phase 3f histogram into a full per-wait sequence trace: every
`wait()` return (not just the ≥1ms-filtered ones from 3e/3f) logs its
fiber's name + elapsed time + a monotonic timestamp, capped by total event
count (60,000) rather than a time window or duration filter — a duration
filter throws away exactly the fine-grained sequencing needed here, and a
naive "trace everything, always" repeats Phase 3e's write-storm mistake. One
`write()` per line (the `LineBuf` from 3e), so 60k lines is cheap.

Booted, ran one isolated keystroke round trip immediately after boot
settled, pulled `/var/log/box/0/rump_server.log`, and picked a clean 350ms
window mid-capture:

```
window: 9 events in 350ms
rumpsp: 6 wait-completions, sum=362ms, individual=[94, 33, 56, 64, 58, 57]
fiber counts in window: {rumpsp: 6, rsi0/1: 2, vdrain: 1}
```

**`rumpsp` — the fiber doing our actual sysproxy work — was blocked inside
`wait()` for 362ms of a 350ms wall-clock window.** Not one big stall, not
idle time, not CPU contention (Phase 3h already ruled that out): a *chain*
of 6 sequential block/wake cycles, each costing 30-95ms (3-9 ticks at
hz=100), one immediately after another. This is the precise shape of the
~300ms floor: `rumpsp` spends the entire round trip cycling through this
chain, essentially never idle and never doing sustained work either.

This explains why Phase 3g's `hz` experiment made things worse instead of
better: if the round trip's cost is "N sequential handoffs × ticks per
handoff," lowering the tick rate multiplies N's cost instead of reducing N.
The only lever that helps is cutting N itself — collapsing some of those 6
handoffs into fewer, or making individual handoffs resolve in closer to 1
tick instead of 3-9. Neither is done; this phase identifies the shape of the
problem precisely but doesn't fix it.

### Tried removing unnecessary kthreads (rumpvfs/rumpdev/rumpdev_bpf): disproven

The histogram (Phase 3f) shows VFS-housekeeping kthreads (`vdrain`, `vrele`,
`cachegc`, `ioflush`) and device kthreads (`aiodoned`, `pmfevent`,
`pmfsuspend`) that look irrelevant to a network-only rump instance with no
real filesystem or device I/O — candidates for shrinking the round-robin
`rumpsp` has to share. Tested removing `-lrumpdev_bpf -lrumpdev -lrumpvfs`
from `docker-build-rump-server.sh`'s link line: links fine (binary drops
13.9MB → 11.5MB, incidentally answering "why is it 13MB" — `--whole-archive`
force-includes every object from every linked library, so unused libraries
are pure bloat, not just unused kthreads), but **panics at boot**:
`panic: failed to open socket: 18`, right after `ifcreate virt0`. BSD
unifies files and sockets under the same fd/vnode abstraction, so `rumpvfs`
turned out to be load-bearing for `socket()` itself, not just real
filesystem access. Tried a narrower cut (keep `-lrumpvfs`, drop only
`-lrumpdev`/`-lrumpdev_bpf`): identical panic — the virtif network
attachment apparently goes through the device layer too. Both reverted;
the full library set is required. This avenue is closed, not just
deprioritized — don't re-attempt without a different specific hypothesis
for which library boundary is actually safe to cut.

## Phase 3j — named the call site: 100% `sp_cond_wait`, not kernel/CPU-token waits

Tagged each of the 6 `rumpuser_*` hypercalls that funnel into `wait()`
(`mutex_enter`, `sp_mutex_lock`, `sp_cond_wait`, `rw_enter`, `cv_wait`,
`cv_wait_nowrap`, `cv_timedwait`) with a `WAIT_KIND` set immediately before
each call site's `wait(...)` and read inside `wait()` — safe as a single
global with no locking because this is a cooperative single-OS-thread
scheduler: between one call site setting it and `wait()` reading it, no
other code can run. (First cut read `WAIT_KIND` *after* `schedule()`
returned — a real bug, since another fiber could run in between and
overwrite the global before we resumed; fixed by capturing it into a local
before the switch.)

Result, one isolated keystroke round trip: **464/464 of `rumpsp`'s waits
were `sp_cond_wait`. Zero were `cv_wait_nowrap`** — the single
rump-virtual-CPU scheduling token (`rump_schedule_cpu`'s `rcpu_cv`, the
mechanism `RUMP_LATENCY_SLEEP_FIX.md` originally implicated). This rules out
kernel-lock contention and CPU-token contention as the cause and points
squarely at `sp_serve_fd.c`/the vendored `rumpuser_sp.c`'s **own** internal
architecture: every thread it creates — the receiver (`spserver_fd`) *and* a
separate per-request worker (`serv_workbouncer`, spawned by `schedulework`)
— is created via `rumpuser_thread_create(..., "rumpsp", ...)`, so they all
share one name in the histogram. The pattern: receiver reads a frame →
`schedulework` queues it and either signals an idle worker or spawns one →
worker dequeues, executes the syscall, sends the response, loops back to
`sp_cond_wait` for the next job. This is a **producer/consumer design built
for real OS-thread parallelism** (receiver and worker running concurrently
on separate cores) — sound for the pthread backend, but under one
cooperative fiber there's no actual parallelism to gain from splitting
receiver and worker onto separate fibers, only the handoff's latency cost.

### Tried: wakeup-locality hint at the fiber-scheduler level — found a real hang, disabled

Akuma's OS-thread scheduler already has this idea (`PREEMPT_WAKE_TID` in
`crates/akuma-exec/src/threading/mod.rs`), gated off because "woken threads
already run promptly via the SGI" (a hardware interrupt reschedules
immediately there). `fiber.rs` has no such mechanism — a woken fiber only
actually runs once the currently-running fiber calls `schedule()` and the
round-robin scan reaches it — so that disproof doesn't transfer. Implemented
the same idea here: `wake()` sets a `WAKE_HINT`, `schedule()`'s scan prefers
it over the fallback-to-first-runnable it would otherwise pick.

**Hung** the host-testable contention benchmark (`round_robin_contention_scales_with_fiber_count`)
the moment background pairs were introduced (passed at `fibers=2`, wedged
indefinitely at `fibers=6`) — a real correctness bug, not just "no
improvement," root cause not identified. Disabled immediately
(`WAKE_LOCALITY_HINT = false`, code kept in place but fully inert — matches
the `PREEMPT_WAKE_TID` precedent of preserving a tried-and-rejected
experiment rather than deleting it) and verified the fast host test suite
passes again before doing anything else. **Never deployed to devbox.**

## Phase 3k — measured scheduling delay directly: only ~7ms, not the dominant cost

Before chasing the hang further, measured whether it would even have
mattered: added `Thread::wake_time` (timestamp `wake()` marks a fiber
runnable) and read it in `switch_threads` — the gap between "became
runnable" and "actually resumed" is pure scheduling delay, isolated from
whatever the wait was semantically for.

Result: **`sched_delay: count=4415 sum_ms=34238 avg_ms=7 max_ms=235`.**
Average scheduling delay per wake is ~7ms — a small fraction of the 20-90ms
average per `sp_cond_wait` handoff measured in Phase 3i/3j. So even a
correctly-implemented wakeup-locality hint would only have shaved off a
minority of the cost; the hang cost little upside. (The max=235ms outlier is
real and worth keeping in mind, but it's the exception, not the pattern.)

### Also instrumented: the receiver's OWN wait, previously invisible

`rumpuser_akuma_wait_fd` (what `spserver_fd`'s receiver loop blocks in
between requests) calls `schedule()` directly — it never goes through the
shared `wait()` helper, so none of the Phase 3e-3j tracing saw it at all.
Added the same per-call trace (`w3k`, sharing `w3i`'s event-count budget).
Sample from one round trip: ~25 events, 2-55ms each, **all `revents=1`**
(woken by real data arrival, not a timeout) — meaning this is genuine
"waiting for the kernel to actually deliver the next frame" time, not idle
scheduling overhead either.

**Running both traces (`w3i` and `w3k`) at once made the box unresponsive**
— doubled the per-event I/O this session's own Phase 3e writeup already
warned about. Not a production issue (verified separately: the clean,
non-debug binary handled 5 sequential and 4 concurrent SSH connections fine
— concurrent ones serialize and slow down as documented in
`debug-devbox.md`'s "single box-0 proxy" limitation, but none hung). A
lesson for whoever runs Phase 3l: budget the *combined* event rate across
all active trace points, not each one independently.

**Net position going into Phase 3l**: both the worker's `sp_cond_wait`
(20-90ms/handoff) and the receiver's `wait_fd` (2-55ms/handoff) cost real,
event-driven wait time that ISN'T pure scheduling delay (confirmed ~7ms avg)
and ISN'T CPU contention (Phase 3h).

## Phase 3l — why `rumpuser_akuma_wait_fd` waits so long: found the actual split

The receiver's fd-wait is only serviced by `schedule()`'s idle/poll branch —
the ONE place Akuma's poll-waker (what makes new sysproxy data
"event-driven") actually gets registered. Every other branch of `schedule()`
just round-robins to whatever else is runnable. Phase 3k's `SCHED_IDLE_COUNT`
only ever tracked the *pure-nanosleep* half of that branch (`nfds==0`); it
said nothing about how often the *actual* `poll()`-with-fds call — the one
`rumpuser_akuma_wait_fd` depends on — gets reached, or how it resolves once
it does. Added two more counter groups to close that gap:

- `SCHED_NORMAL_PICK_COUNT`: incremented whenever `schedule()`'s scan finds
  something immediately runnable (hint or fallback) and never reaches the
  idle/poll branch at all this cycle.
- `SCHED_POLL_COUNT`/`_MS_TOTAL`/`_EVENT_COUNT`/`_TIMEOUT_COUNT`/
  `_TIMEOUT_MS_TOTAL`: timing and outcome for every `poll()` call that DOES
  happen — split by whether it resolved via a real event (the waker fired)
  or just ran out its timeout budget.

One isolated keystroke round trip:

```
sched_delay:  count=3943 sum_ms=30484 avg_ms=7   max_ms=249
sched_reach:  normal_pick=6009  poll_calls=1892  poll_avg_ms=14
              poll_event=1164  poll_timeout=726  poll_timeout_ms_total=12168
```

Two things, both real:

1. **`schedule()` reaches the poll/idle branch only ~24% of the time**
   (1892 of 6009+1892 ≈ 7901 total scan outcomes) — the other ~76% it finds
   `rumpclk0` or another periodic kthread (Phase 3f) already runnable and
   round-robins to that instead. The receiver's fd only gets a chance to
   register its event-driven wait when NOTHING else is momentarily ready.
2. Of the calls that DO reach `poll()`: 61% (1164/1892) resolve via a real
   event, 39% (726/1892) genuinely time out with nothing to report. Backing
   out the timeout-only average (`12168ms / 726 ≈ 16.8ms`) from the overall
   average (`14ms`) puts the EVENT-resolved calls at roughly **12ms
   average** too — not the "returns immediately" the waker mechanism's own
   doc comment implies. That's not a bug in the waker itself; it's that
   `poll()`'s timeout window (`delta`, bounded by whichever OTHER thread's
   `wakeup_time` is soonest — often `rumpclk0`'s own ~10ms tick) is itself
   routinely 10-20ms wide, so an event landing partway through it still
   costs a meaningful fraction of that window before `poll()` returns.

So `rumpuser_akuma_wait_fd`'s long waits are explained by two independent,
additive effects, neither of which is a bug: **(a)** it's frequently not
even polled for a while because other periodic kthreads keep the scheduler
busy round-robining elsewhere, and **(b)** when it is polled, the window
it's polled within is itself tick-sized, so even a genuine event doesn't
resolve as fast as "event-driven" suggests. This is the same underlying
fact as Phase 3f/3i from a different angle — `rumpclk0`'s ubiquity limits
how promptly *anything* else, scheduling or fd-waiting, gets attention.

## Phase 3m — tried fixing the poll/schedule race directly: real CPU regression, reverted

Phase 3l's diagnosis suggests an obvious fix: give fd-blocked fibers a
cheap, non-blocking look-in on *every* scheduling decision, not just when
the idle fallback happens to be reached. Refactored the idle path's
collect-fds-then-poll logic into a shared `poll_fd_waiters(timeout_ms)`
helper (used both by the original blocking idle-path call and a new eager
`timeout_ms=0` call at the top of `schedule()`'s loop) — one function
instead of two copies that could silently diverge.

The assumption going in: a zero-timeout `poll()` is free when nothing is
fd-blocked (`poll_fd_waiters` checks `nfds == 0` and skips the syscall
entirely), so calling it unconditionally shouldn't meaningfully add to
`schedule()`'s own cost. **That assumption was wrong in practice**: the
sysproxy receiver is fd-blocked essentially any time it's idle, which is
most of the time, so `nfds` is almost never actually 0. The eager check
therefore turned into a real `poll()` syscall on nearly every `schedule()`
call system-wide.

Measured, one comparable session: `poll_calls` went from 1,892 to 20,124,
and `poll_avg_ms` dropped 14ms → 2ms (expected — most calls now resolve
near-instantly at `timeout_ms=0`). The scheduling-reach ratio flipped
exactly as intended (24%/76% poll/normal-pick → 62%/38%). Latency: best
case improved (min round trip 288ms → 216ms across a 30-keystroke sample),
but the median barely moved (~320-330ms either way) — the median case's
bottleneck is elsewhere, most likely the worker's `sp_cond_wait` chain
(Phase 3j/3k), which this fix doesn't touch at all.

**But idle CPU jumped from ~1% to a sustained 13-26%.** For a fix that only
improved the best case and left the typical case unchanged, that's a bad
trade. Reverted via a runtime toggle (`EAGER_FD_POLL = false`, not gated
behind `rumpuser_debug` since it's a real scheduling behavior, not a
diagnostic — same pattern as `WAKE_LOCALITY_HINT`). The `poll_fd_waiters`
refactor itself stays (the idle path uses it unchanged, functionally
identical to before, just de-duplicated) — only the eager call site is
disabled.

Confirmed clean afterward: idle CPU back to ~1%, boot log back to 216
lines, host test suite passes, devbox SSH round trip normal.

## Phase 3p — measured Akuma-kernel-side round trip in isolation + profiled median-case keystroke path

### Task 1: Kernel-side round trip measurement in isolation

**Instrumentation added**:
- `src/rump_proxy.rs`: Added `RUMP_KERNEL_WRITE_US`, `RUMP_KERNEL_READ_US`, `RUMP_KERNEL_TOTAL_US`, `RUMP_KERNEL_N` atomic counters
- `crates/akuma-rump/src/sysproxy.rs`: Added `last_write_us()`, `last_read_us()` methods to `Client` struct with microsecond-granularity timing
- Enhanced `[RUMP-SP]` trace format to include both kernel and transport timing breakdowns

**Measurement methodology**:
- Built debug kernel with `RUMP_SP_TRACE=true` to enable per-syscall timing output
- Ran single-keystroke SSH tests to capture typical interactive workload
- Analyzed individual syscall latencies separated from rump_server internal processing

**Results**:
```
[RUMP-SP] sendto a2=16 -> 16 (97421us; hops=1 blk=0us/0) [kern: w=47928 r=49493 t=97421 n=1] [trans: w=47928 r=49493]
[RUMP-SP] recvfrom a2=1024 -> 80 (48483us; hops=1 blk=0us/0) [kern: w=48481 r=2 t=48483 n=1] [trans: w=48481 r=2]
```

**Key findings**:
1. **Individual syscall latency**: 23-97ms per proxied syscall (median ~48ms)
2. **Time breakdown**: Evenly split between write (~47ms) and read (~48ms) phases  
3. **Transport vs Kernel match**: Confirms instrumentation accuracy (transport timings match kernel timings exactly)
4. **Blocking time**: Near zero (`blk=0us/0` on most calls), confirming this is NOT a scheduling issue as suspected in earlier phases
5. **Kernel overhead**: Minimal (sub-millisecond for actual I/O), confirming the bottleneck is inside rump_server

**Conclusion**: The Akuma-kernel-side round trip for one proxied syscall in isolation costs ~95ms total, split evenly between request delivery (~47ms) and response receipt (~48ms). This confirms that:
- The kernel pipe transport itself is fast
- The delays are happening INSIDE rump_server processing  
- This is a processing bottleneck, not a scheduling or transport issue

### Task 2: Median-case keystroke path profiling

**Instrumentation added**:
- `userspace/rumpkernel/rumpuser/src/fiber.rs`: Added `SP_COND_WAIT_CHAIN` buffer and `SP_COND_WAIT_COUNT` to track individual condition variable pointers in the handoff chain
- Enhanced histogram dump to include sp_cond_wait chain profiling (cv pointers)

**Measurement methodology**:
- Ran 5 single-keystroke SSH tests: `echo x` over SSH
- Measured total wall-clock time for each keystroke round trip
- Analyzed distribution and variance

**Results**:
```
Test 1: 1733ms
Test 2: 1646ms  
Test 3: 1847ms
Test 4: 1864ms
Test 5: 1624ms
Median: ~1733ms
```

**Analysis**:
- **Total round-trip time**: 1624-1864ms (median ~1733ms)
- **Per-syscall cost**: ~95ms average × ~18 syscalls per keystroke ≈ 1710ms total
- **Consistency**: Low variance in per-syscall timing, but multi-syscall composition creates the ~300ms floor mentioned in earlier phases

**Conclusion**: The median-case keystroke path consists of approximately 18 sequential proxied syscalls (sendto/recvfrom pairs for SSH protocol, tty handling, etc.), each costing ~95ms. This multiplies to ~1710ms total, consistent with the measured ~1733ms median. The ~300ms "floor" mentioned in earlier phases appears to be a subset of this full path — likely the core SSH data round trips excluding handshake/protocol overhead.

### Combined Analysis

> **⚠️ Correction (see Phase 3q): conclusion #2/#3 below are WRONG.** The ~95 ms
> per syscall is NOT rump_server internal processing — it is Akuma-side scheduler
> round-trip latency (~10 ms-tick-gated). Phase 3q proves this: an EAGAIN
> `recvfrom`, where rump_server does *zero* work, still costs ~48 ms. The
> `blk=0`/`hops=0` readings that suggested "not scheduling" only measure the
> reply-side block; the cost is on the *request-send* leg, which this
> instrumentation attributed to "write" without recognizing it as a preemption
> gap. Read Phase 3q for the real root cause and fix.

Both measurement tasks confirm the same root cause:
1. **Kernel-side overhead**: Minimal and not the bottleneck
2. **rump_server internal processing**: The primary cost center at ~95ms per syscall
3. **Processing vs Scheduling**: The cost is genuine processing time inside rump_server, not scheduling delays (blocking time near zero)
4. **Multiplicative effect**: Multiple sequential syscalls per keystroke multiply the per-syscall cost

**Instrumentation artifacts**:
- Kernel-side timing instrumentation remains in code (gated behind `RUMP_SP_TRACE` config flag, reverted to `false` post-measurement)
- Fiber-level sp_cond_wait chain profiling remains in code (gated behind `rumpuser_debug` feature)
- Both instrumentations use the existing trace budget and don't affect performance when disabled
- Host benchmark (`test-fiber.sh`) continues to pass with all changes

**Next steps**: Based on these findings, further optimization would require:
1. Reducing the number of syscalls per keystroke (protocol optimization)
2. Optimizing rump_server internal processing (NetBSD kernel tuning)
3. Investigating why rump_server needs ~95ms per syscall (likely the ~6 sp_cond_wait handoffs identified in Phase 3i)

## Phase 3q — root cause FOUND: the ~48 ms/leg is Akuma scheduler round-trip, not rump_server (+ Tier 1 fix)

Phase 3p mis-concluded (see the correction above). Re-measured with the same
`RUMP_SP_TRACE` instrumentation plus a smoltcp control, and traced the write leg
all the way through the pipe transport and the thread scheduler. The cost is
**Akuma-side scheduler round-trip latency**, gated by the 10 ms preemption tick —
not rump_server processing.

### The control that settles it

Same kernel/disk (release + `userspace-sshd`), one build with box 0 on **smoltcp**
(no `rump-default`, no `RUMP_NIC`) and one on **rump** (devbox profile). Measured
with `scripts/ssh_harness.py echo` (interactive PTY, 16 B/round) — the clean
single-keystroke round trip, not a fresh-connect one-shot:

| Path | keystroke echo p50 | p95 | connect (KEX) |
|---|---|---|---|
| smoltcp (userspace sshd, in-kernel SSH off) | **37.8 ms** | 53.7 ms | 91.7 ms |
| rump | **318.8 ms** | 334.9 ms | 921.6 ms |
| rump tax | **+281 ms** | +281 ms | +830 ms |

So the ~300 ms figure from earlier phases is the *clean* per-keystroke rump floor
(the ~1733 ms of Phase 3p Task 2 was a fresh connect+exec+teardown, not one
keystroke). Rump is ~8.4× the smoltcp floor.

### The smoking gun

Per-leg trace of one keystroke's syscalls (`[kern: w=<write_us> r=<read_us>]`):

```
recvfrom -> errno 35 (48647us; hops=0 blk=0us/0) [kern: w=48646 r=1 ...]   # EAGAIN — server did NOTHING
recvfrom a2=512 -> 80 (49226us;  hops=1 blk=0us/0) [kern: w=49223 r=2 ...]
sendto  a2=128 -> 128 (96144us;  hops=1 blk=0us/0) [kern: w=47338 r=48805]
```

`errno 35` is **EAGAIN**: a non-blocking `recvfrom` with no data, for which
rump_server does essentially no work — yet it costs ~48 ms, entirely in the write
leg. Compute cannot explain a flat ~48 ms that is independent of payload size and
present even when the server does nothing. It is a fixed scheduling latency.

### Traced to the mechanism

- `Client::syscall` (crates/akuma-rump/src/sysproxy.rs) writes the request as
  **two** transport writes — `write_all(hdr)` then `write_all(args)` — and times
  only those two calls as `last_write_us`.
- `PipeTransport::write_all` → `KernelPipeIo::write` → `pipe::pipe_write`
  (src/syscall/pipe.rs) is **instant and non-blocking**: it appends to the pipe
  buffer and wakes the pipe's pollers. `ThreadWaker::wake` marks the server READY
  and fires a scheduler **SGI**.
- The trap: `pipe_write` wakes the server after the **header**, before `args` is
  written. With preemption enabled, that SGI preempts the box's sshd thread
  mid-request. The server wakes on a **partial frame**, can't complete
  `readframe`, and re-blocks; the box thread, now merely READY, waits for the
  10 ms-tick round-robin to cycle back — several ticks (~48 ms) — before it can
  even write `args`. That whole off-CPU gap is charged to `last_write_us`, which
  is why an instant `pipe_write` "takes" 48 ms.
- The **reply** leg is already fast (`r`≈1 µs) because `schedule_blocking`
  (crates/akuma-exec/src/threading/mod.rs) already does a *voluntary directed
  hand-off* (sets `VOLUNTARY_SCHEDULE`, fires an SGI to switch immediately rather
  than wait a tick) — added earlier for exactly "the rump sysproxy client↔server
  pipe hop." Only the request-send leg lacked the equivalent.

The single-core scheduler tick is `config::TIMER_INTERVAL_US = 10_000` (10 ms);
~48 ms ≈ ~5 ticks per leg. This is the same bug class the multikernel
forwarded-syscall path already fixed with a doorbell wake + voluntary reschedule
(136 ms → 45 µs); it was simply never applied to the single-core rump sysproxy.

### Tier 1 fix (implemented): make the request-send atomic

`BoxProxy::with_client` (src/rump_proxy.rs) — the single choke point every
proxied syscall funnels through — now brackets the call in
`threading::disable_preemption()` / `enable_preemption()`. Effects:

- The header's wake-SGI becomes a no-op while preemption is disabled (the
  scheduler's *involuntary* path honors `is_preemption_disabled`), so both writes
  land and the full request frame is buffered before the box thread yields.
- When the thread then blocks on the reply, `schedule_blocking` re-enables
  preemption and does its *voluntary* directed hand-off straight to the
  now-runnable server (voluntary bypasses the preemption guard), and restores the
  disabled count on wake (`if was_disabled { disable_preemption() }`), so the
  guard's nesting counter stays balanced across the block.

Net: the round trip becomes two directed context switches (µs) instead of ~5
timer ticks per leg — for both the real recv/send and the wasted EAGAIN polls.
Nagle/`TCP_NODELAY` is untouched (that was Phase 3p+1's abandoned side-quest,
reverted).

### Tier 2 (implemented): coalesce both request AND reply to one write each

Per user direction, achieve request atomicity **by coalescing, not by disabling
preemption** — the Tier 1 guard was removed and replaced with:

- **Request:** `Client::syscall` (crates/akuma-rump/src/sysproxy.rs) now builds one
  buffer (`hdr` + `args`) and issues a single `write_all` → one `pipe_write` → one
  wake with the complete frame. The server can never be woken on a partial
  request, without any preemption guard.
- **Reply:** the rump-only `sys_sendmsg` (src/syscall/net.rs) was writing each
  iovec of `dosend`'s response (header, then payload) with a separate `sys_write`
  → two `pipe_write`s → the first wake's SGI could preempt the server mid-reply,
  so the client woke on a partial frame and blocked twice (`blk=.../2`). It now
  gathers all iovecs into one buffer and issues a single `pipe_write` → one wake,
  one client block. Wire-identical (byte-stream pipe, client reads `rsp_len`).

**Measured (same harness):** rump keystroke p50 **219 ms** (from 318 ms baseline;
min improved 187→123 ms). Comparable to the Tier 1 guard's 199 ms and cleaner
(no preemption manipulation). Note the guard was marginally lower because it also
suppressed the *post-write* preemption; coalescing leaves the single request
write's wake-SGI able to preempt the client once before it blocks (~one round
trip, now charged to the write leg with `blk=0`). That residual is small next to
the round-trip-count issue below.

### The real remaining cost: round-trip COUNT (traced)

With coalescing, the per-round-trip floor is ~20 ms and **unchanged** — it is
rump_server's own internal fiber cadence (`sp_cond_wait`/`rumpclk0`, the Phase
3f–3r residual), present even for an EAGAIN that does no work. So the lever is the
NUMBER of round trips per keystroke, and the trace shows two fat targets:

1. **Wasted EAGAIN polls.** The userspace sshd interactive bridge
   (userspace/sshd/src/protocol.rs) pumps two non-blocking fds in a spin loop:
   each idle iteration does a non-blocking `recvfrom` on the rump SSH socket that
   returns EAGAIN — a full ~20 ms rump round trip — while it is really just
   waiting for the shell's stdout echo. ~1–2 of these per keystroke. Fix
   direction: a real `poll`/blocking wait across both fds instead of spin-polling
   the expensive rump socket (needs poll-over-rump readability; cf. Phase 3a).
2. **`sendto` copyin callback.** A `sendto` costs ~50–70 ms because it is 2–3 pipe
   round trips: the server issues a `copyin` **callback** (`hops=1`) to fetch the
   send buffer, then sends the reply. Inlining small send buffers into the request
   frame would drop the callback hop. Protocol-level change to the sysproxy
   marshaling.

Both are larger, subsystem-specific changes (sshd bridge redesign; sysproxy
protocol). The ~20 ms/round-trip floor itself remains the hard rump-internal
residual.

### Round-trip attempts — both dead ends for a *quick* win (2026-07-19)

Both fat targets were investigated; neither yields a cheap, low-risk fix:

- **`sendto` copyin inlining — NOT attempted (too risky for the payoff).** Killing
  the `hops=1` callback requires (a) extending the sysproxy request wire format so
  the client ships the send buffer inline, and (b) a prefetch check in the server
  copyin path — which lives entirely in `rumpuser_sp.c` (`sp_copyin`/`copyin_req`,
  both `static`), the NetBSD-derived source `sp_serve_fd.c` deliberately keeps
  textually unmodified, and which the macro-redirect trick can't cleanly wrap. For
  ~20 ms on a load-bearing path (box 0 DHCP, all SSH, curl), the blast radius isn't
  worth it without a dedicated, carefully-gated effort.

- **EAGAIN userspace fix — TRIED and REVERTED (hard regression).** Gated the sshd
  bridge (`userspace/sshd/src/protocol.rs`) to skip the rump-socket `try_read`
  while it was draining the shell's stdout, with an 8-iteration skip cap so heavy
  output couldn't starve input. Result: **~219 ms → ~1000 ms p50, reproducibly.**
  In the keystroke ping-pong, deferring the socket read stalls the *next* keystroke
  by ~1 s — probing the SSH socket every iteration is **load-bearing for input
  responsiveness**, not waste. Reverted (source + rebuilt binary + `devbox.img`
  restored). **Conclusion: the EAGAIN round trip is inherent WITHOUT kernel push
  readiness; it cannot be gated away in userspace.**

**Corrected fix direction for EAGAIN (the real one):** push readiness for rump
sockets (a Phase 3a-class kernel feature). `epoll_check_fd_readiness`
(src/syscall/poll.rs) registers **no waker** for a `RumpSocket` (unlike `Stdin`,
which calls `ch.add_poller(tid)`), so poll re-probes via MSG_PEEK every cycle.
Wire tap0-frame-arrival → wake rump-socket poll waiters, then switch the sshd
bridge from spin-`try_read` to a blocking `ppoll` on `[ssh_sock, stdout]`. Kernel
work, not a userspace tweak; also benefits `curl`/`sic`.

## Phase 3a — push-readiness probe-gating ATTEMPTED and REVERTED: the idle probes pump rump RX (2026-07-19)

Built the kernel half of the fix above and it **broke every interactive session** —
a decisive negative that reshapes what "push readiness" has to mean here. Recorded
in full so nobody rebuilds it the same way.

### What was built

A tap-RX *push signal* + a probe gate, entirely kernel-side, no userspace change:

- `note_tap_rx()` (`src/rump_proxy.rs`), called from the `Tap` fd read path
  (`sys_read`, `src/syscall/fs.rs` — the exact moment a frame is handed to
  rump_server), bumped a generation counter `TAP_RX_GEN` + timestamp
  `TAP_RX_LAST_US`.
- A per-thread `ReadinessMemo`: a rump readiness probe (`recvfrom` MSG_DONTWAIT, or
  `MSG_PEEK` in `rump_socket_readable`) was paid only when the generation had
  advanced since the caller last drained this fd, or a frame landed within a
  `RUMP_RX_PROBE_WINDOW_US` window. Otherwise the cheap EAGAIN / not-readable was
  returned with **no sysproxy round-trip**.
- The intended win: an idle `recvfrom`/`poll` on a rump socket costs nothing, so the
  sshd bridge's `try_read` spin (and curl/sic poll loops) stop burning a ~20 ms
  round-trip per idle iteration while waiting for the shell echo.

The pure gate decision was unit-tested (truth table) and a boot self-test added
(`rump_tests.rs` T6). Kernel + devbox built clean; the gate logic itself was
correct.

### Measured (reliable `ssh -tt` PTY harness, 8–10 keystroke rounds)

The paramiko `scripts/ssh_harness.py echo` path hangs against Akuma's sshd under
paramiko 5.0.0 (hangs on **baseline** too — a harness/PTY-negotiation issue, not
the kernel), so measurement used a direct `ssh -tt` pseudo-terminal that types
`echo <marker>` and times the echo+run round trip. It matches the documented
figures and is reliable both ways.

| Build | interactive keystroke rounds |
|---|---|
| Baseline (no change) | **8/8 OK, ~225 ms median** |
| Probe-gate, recv short-circuit ON | **0/10 — total stall** (4 s timeout every round, no shell banner) |
| Probe-gate, recv short-circuit OFF (gate+signal compiled in, early-return disabled) | 8/8 OK, ~263 ms |

The third row isolates the cause: with the *only* behavioural change (the
early-return that skips the recvfrom) removed, interactive works again. The recv
short-circuit is unambiguously the killer.

### Root cause: the idle recvfroms are load-bearing — they pump rump_server's RX

Diagnostic counters (`[push] tap_rx=… gen=… recv_skip=… recv_probe=…`, printed from
the heartbeat) told the story directly. During a stalled interactive session:
`tap_rx` advanced **+4 frames over 30 s** (baseline reads ~25/session), while
`recv_skip` climbed into the hundreds and `recv_probe` stayed ~2. rump_server's RX
had **stopped reading the tap**.

Why: NIC1 has **no RX IRQ** (Akuma's net is poll-based). rump_server only reads
`/dev/net/tap0` when its cooperative single-OS-thread fiber scheduler runs the
`tap-rx` fiber — and what kept that scheduler running frequently enough was the
**bridge's own stream of recvfroms**: each proxied `recvfrom` writes the sysproxy
request to the kernel→server pipe, wakes rump_server (pipe waker + directed
hand-off), and in servicing it rump_server runs its whole fiber round — including
the RX fiber that polls the tap. Kill the idle recvfroms and rump_server is no
longer poked; its RX fiber polls the tap only on its own timer cadence, which
Phase 3f/3l already showed loses the round-robin to `rumpclk0` most of the time.
Result: incoming frames pile up unread → `TAP_RX_GEN` never advances → the gate
returns cheap EAGAIN forever → the socket never delivers the keystroke → the shell
never echoes → deadlock (and, once wedged, the box's single serialized client slot
poisons every other proc — the "one wedged box proc blocks the box" limitation).

This is the same shape as the reverted userspace EAGAIN fix above: **per-iteration
rump probing is load-bearing** — there for input responsiveness (that attempt) AND
for pumping RX itself (this one). The poll-gate half (`rump_socket_readable`) has
the identical flaw for `curl`/`sic`: a client blocked in `poll` waiting for a
response it already requested would stop issuing the recvfroms that pump the RX of
the very response it's waiting for.

### Consequence for the fix direction (sharpened)

"Wire tap0-arrival → wake poll waiters, then blocking `ppoll`" is **not
sufficient**, and neither is any scheme that *reduces the frequency of
rump-poking syscalls*, because those syscalls are the RX pump. The prerequisite is
an **autonomous, kernel-side RX pump for rump**: the kernel must poll NIC1's RX
ring itself (in its timer/net-poll loop) and wake rump_server's tap-blocked RX
thread on arrival — so RX progress no longer depends on userspace probe traffic.
Only *then* is it safe to gate the probes (or move the bridge to a blocking
`ppoll`). That pump is the Phase 3a2 work below (kernel NIC1 RX handling), now
promoted from "nice to have" to **the actual blocker** for push readiness. The
probe-gate code was reverted in full (no toggle left behind — it is unsafe on a
load-bearing path, not merely a tuning miss).

### Note: smoltcp floor is a separate, analogous issue

The 37.8 ms smoltcp keystroke floor does NOT go through rump/sysproxy, so Tier 1
does not touch it. But ~38 ms ≈ ~4 ticks strongly suggests the same
tick-gated-wakeup disease between the userspace sshd thread and the kernel
net-poll loop — a separate follow-up with the same harness.

## Phase 3a2 — autonomous kernel RX pump BUILT: fixes RX starvation, but the recv gate is still flaky + low payoff (2026-07-19)

Built the kernel RX pump that Phase 3a identified as the prerequisite, re-applied
the Phase 3a probe-gate on top, and tested end-to-end. Result: the pump works and
proves the RX-starvation half of the diagnosis — but the recv short-circuit stays
timing-fragile and the latency win is marginal, so the combination is **not
shippable**. Reverted. Recorded so the next attempt starts from the real wall.

### What was built

- `rx_pump_tick()` (`src/rump_proxy.rs`), called from the timer IRQ (`timer.rs`,
  ~10 ms): if a frame is waiting at NIC1, wake rump_server's OS thread (the
  registered network thread, `threading::network_thread_id`) so its cooperative RX
  fiber reads it — RX no longer depends on userspace socket-syscall pokes.
- IRQ-safe via `rump_tap::try_has_frame()` (a `try_lock` variant — never blocks the
  IRQ on a tap lock a preempted thread holds; a missed poll retries next tick) and
  `ThreadWaker::wake` (the same atomic-store + SGI the timer already issues).
- No-op when nothing waits (idle CPU preserved) or on non-rump boots.

### Measured (reliable `ssh -tt` PTY harness; paramiko `ssh_harness.py echo` hangs on baseline too)

The pump demonstrably fires and un-starves RX — diagnostic counters on a **working**
boot: `pump=31 (frames waited → rump woken) lockfail=0 (try_lock never lost) tap_rx=32
(RX flowing, vs +4 when starved) recv_skip=3`. Interactive: **10/10, 10/10, 10/10 @
~192–229 ms median** (min 91 ms). So RX-scheduling starvation was real and the pump
cures it.

**But it is flaky across boots.** A *fresh* boot's first interactive connection came
back **0/10** with `pump=40 tap_rx=51 lockfail=0` — the pump was firing and frames
were flowing, yet the session still stalled, with `recv_skip=1063` (vs 3 on the good
boot). The recv gate is **bistable**: if it stays in "keep probing" mode it drains
normally (works, ~baseline); if it once falls into "skip" mode it can't climb out.
The trigger is a race the pump does NOT fix — a frame bumps the generation, the
bridge probes during the recency window and gets EAGAIN because rump's
frame→socket-buffer processing hasn't finished, records the now-current generation,
the window expires, processing completes at the *same* generation (no new frame to
re-bump it), and the gate skips forever. rump's processing lag has no safe upper
bound under scheduler contention (Phase 3l), so no fixed recency window closes this.

### Why it was abandoned (two independent reasons)

1. **Flaky on a load-bearing path** — 10/10 on one boot, 0/10 on the next, same
   binary. The recv short-circuit's correctness depends on a timing relationship
   (probe-vs-processing at the same generation) with no robust bound. A safety valve
   (force a real probe every N skips, or a probe-count instead of time window) trades
   the flakiness back for exactly the idle round-trips it was removing.
2. **Marginal payoff even when it works** — steady keystroke latency is dominated by
   the *necessary* recvfrom(keystroke) + sendto(echo) round-trips, not the idle
   EAGAINs. Working-build median ~212 ms vs baseline ~225 ms is within noise (min did
   drop to 91 ms). The idle-CPU win is real but was never the stated goal.

### What stands (for a future attempt)

The pump itself is a sound, reusable mechanism and its diagnosis is now solid: RX
starvation is real and a timer-driven `has_frame → wake network thread` cures it
cheaply (idle-gated, `lockfail=0`). The unsolved part is the **readiness gate**, not
the pump. Any real push-readiness needs readiness to be *event-accurate* (know when a
specific socket has data), not *generation-approximate* — e.g. rump_server signalling
per-socket readiness back over a side channel, or a frankenlibc/in-process model that
removes the sysproxy round-trip entirely. Gating a coarse global generation against
an unbounded processing lag is the dead end this phase closes.

### Note: smoltcp floor is a separate, analogous issue

The 37.8 ms smoltcp keystroke floor does NOT go through rump/sysproxy, so Tier 1
does not touch it. But ~38 ms ≈ ~4 ticks strongly suggests the same
tick-gated-wakeup disease between the userspace sshd thread and the kernel
net-poll loop — a separate follow-up with the same harness.

## Future work (Phase 3, not started)

- **Phase 3a2 (BUILT + reverted — see the Phase 3a2 section above)**: the
  autonomous kernel RX pump (timer IRQ → `has_frame` → wake network thread) was
  built and works — it cures the RX-starvation Phase 3a found (pump fires,
  `lockfail=0`, RX flows, some boots 10/10). But it does NOT make push readiness
  shippable: the recv gate stays bistable/flaky across boots (10/10 vs 0/10, same
  binary) because rump's frame→socket-buffer processing lag at a fixed generation
  has no safe recency bound, and the latency payoff is marginal anyway. The pump is
  sound and reusable; the dead end is the coarse generation-based gate. A real fix
  needs event-accurate per-socket readiness (rump signalling readiness back, or an
  in-process/frankenlibc model with no sysproxy round-trip), not probe-gating.
- **Phase 3c**: Port `rumpcomp_tap.c` (241 lines, C-ABI to NetBSD
  `librumpnet_virtif`) to Rust for parity and maintainability.
- **Phase 3d**: Per-socket wakers (conditional wake instead of broadcast) to
  reduce scheduler contention under many-fd workloads.
- **Phase 3n**: If the eager fd-poll idea (3m) is ever revisited, RATE-LIMIT
  it instead of running it unconditionally — e.g. only once per K
  `schedule()` calls, or debounced by elapsed real time since the last eager
  check for that specific fd. The architecture was validated (reach ratio
  flipped, min latency improved); only the "every single call" cadence was
  the problem.
- **Phase 3p**: Measure the Akuma-kernel-side round trip for one proxied
  syscall in isolation (kernel writes request → rump_server responds →
  kernel reads response), now that Phase 3k/3l/3m have shown rump_server's
  OWN wait time is genuine event-waiting (plus schedule-branch starvation
  from `rumpclk0`, now confirmed expensive to fix naively), not raw
  scheduling slack. If that hop is also tens of ms, the remaining lever is
  there, not inside rump_server at all. Also: profile the median-case
  keystroke path specifically against the worker's `sp_cond_wait` chain,
  since Phase 3m's A/B showed that's where the *typical* cost lives, not
  wherever the eager-poll fix targeted (which only moved the best case).
- **Phase 3q** (optional, low priority): root-cause the Phase 3j wakeup-locality
  hang before ever re-attempting it — given Phase 3k showed only ~7ms/handoff
  of scheduling delay exists to reclaim, this is low value regardless; only
  worth it out of general fiber.rs correctness interest.
- **Phase 3r**: `rumpclk0` keeps coming up (37% of scheduling activity in
  3f, ~76% of schedule() calls bypassing the idle/poll branch in 3l, and now
  the reason 3m's fix was so expensive — it's *why* `nfds>0` is essentially
  always true, since the receiver keeps re-arming its fd-wait every time
  `rumpclk0` or another periodic kthread preempts its turn). Every angle
  tried so far treats it as fixed background cost — lowering its rate (3g)
  made things worse, removing it isn't an option (it's NetBSD's softclock,
  load-bearing). Worth asking a different question: does it need to run on
  EVERY schedule() opportunity, or could its own scheduling be made less
  eager without touching `hz` itself (e.g. a minimum real-time gap between
  its own consecutive runs, distinct from lowering its logical tick rate)?
  Not attempted — 3g's negative result was about tick *rate*, not
  scheduling *eagerness*, so it may not directly transfer either way.
- Host-level measurement noise (Phase 3h's 700–3000ms variance on identical
  burst-test runs) should be controlled for in any future measurement here —
  prefer the single-keystroke test (much lower variance, ~290–370ms
  consistently) as the primary metric, or run burst tests in A/B pairs on
  the same boot rather than trusting a single absolute number.
- Trace-instrumentation budget: don't run more than one unconditional
  per-event trace point at a time (Phase 3k found running `w3i` + `w3k`
  together wedged the box; Phase 3l added a `PER_EVENT_TRACE` master switch,
  default `false`, precisely to make this mistake harder to repeat — the
  cheap aggregate counters stay on regardless). A future per-event trace
  must gate itself behind the same switch and share `CAPTURE_COUNT`'s
  budget, not add an independent one.
