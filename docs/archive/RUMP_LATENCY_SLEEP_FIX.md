# Rump sysproxy latency — why the kthreads "spin" and the plan to fix it

> ## ⛔ DEAD END — hypothesis DISPROVEN (2026-06-23)
>
> **The heartbeat/thundering-herd hypothesis below was implemented, measured, and
> falsified. Do not pursue it again.** Steps 1 (lower `hz`) and 2 (`cv_broadcast`→
> `cv_signal`) were built and A/B-tested in-VM against the live `curl`-over-rump path
> (`box use rumpnet -i /bin/curl -sS -H Host:ifconfig.me http://34.160.111.145`, same
> host, `MEMORY=1024M RUMP_NIC=1`, returns the IP every time):
>
> | Config | `hz` | CPU-release wake | curl avg |
> |---|---|---|---|
> | **Baseline (unpatched)** | 100 | `cv_broadcast` | **29.1 s** (29.0–29.3) |
> | Both patches (Step 1+2) | 20 | `cv_signal` | 32.7 s (31.5–33.3) |
> | Step 2 only | 100 | `cv_signal` | 35.1 s (33.6–38.4, n=6) |
>
> **Both levers REGRESS latency; neither helps.** Step 1 *mechanically* worked — at
> `hz=20` the heartbeat dropped 5× (`clock_gettime` 103/s→24/s in PSTATS) and the
> rump_server idle syscall rate halved (210/s→110/s) — yet curl latency did not
> improve, it got *worse*. So the 100 Hz heartbeat and the kthread thundering-herd
> are **not** the dominant cost. Step 2 (`cv_signal`) is actively harmful (+6 s at
> equal `hz`): forcing one-at-a-time baton hand-off (and, in the lost-wakeup-safe
> variant, taking the scheduler mutex on *every* CPU release) serializes the curl
> hot path worse than broadcast's wake-all.
>
> **Where the time actually is:** ~29–35 s for a handful of proxied syscalls ≈ 3–4 s
> each — but that cost lives in the **synchronous round-trip through the Akuma-kernel
> sysproxy channel + Akuma-side scheduling + real TCP RTTs to the internet**, none of
> which the rump heartbeat or herd touches. This matches the earlier dead-end
> ("scheduler wakeup-locality hint — no help", `threading/mod.rs`).
>
> **Lesson:** the plan's own **Step 0 gate** ("numbers match the model → proceed;
> they don't → re-investigate before touching the stack") was the right call and
> would have caught this. The next real lever is to instrument the *Akuma-side*
> proxy round-trip (`src/rump_proxy.rs`: channel read/write, `ProcMem` copyin/copyout,
> scheduler hops), **not** anything inside `rump_server`. The patches were reverted;
> only the `src-netbsd` submodule wiring was kept.
>
> The original plan is preserved below for the record.

---

**Context:** M2 works but each proxied syscall costs ~0.8–4 s; full curl ~26 s
(`RUMP_SYSPROXY.md` "B-OUTCOME", `HANDOFF.md` "NEXT TASK"). `ps` shows the
`rump_server`'s ~19 rump kthreads as `STATE running, SYSCALL -` (runnable in
userspace, **not** blocked in a syscall); only the main thread sits in a futex.
Removing a duplicate second `rump_server` *halved* per-op latency — i.e. the cost
is linear in the kthread count. This doc explains the mechanism and lays out the
fix.

---

## Why we're "spinning" — it's not a tight spin, it's heartbeat-driven herd churn

I traced every blocking primitive. **None of them busy-spin.** The contention is an
emergent property of three things interacting:

### 1. The single rump virtual CPU

The rump kernel runs `ncpu == 1`. Every kthread that wants to execute kernel code
must first acquire the one virtual CPU via `rump_schedule_cpu`
(`src-netbsd/sys/rump/librump/rumpkern/scheduler.c:290`). When the CPU is busy, the
waiter blocks correctly:

```c
/* scheduler.c:360 */
rcpu->rcpu_wanted++;
rumpuser_cv_wait_nowrap(rcpu->rcpu_cv, rcpu->rcpu_mtx);   // → pthread_cond_wait (real futex block)
rcpu->rcpu_wanted--;
```

Our `rumpuser_cv_wait_nowrap` (`rumpuser/src/lib.rs:744`) is a genuine
`pthread_cond_wait`. So a waiter *does* block — the problem is it doesn't *stay*
blocked.

### 2. The 100 Hz heartbeat keeps re-waking the whole herd

The `doclock` kthread (`intr.c:117`) is an infinite loop: run `hardclock()`, then
`rumpuser_clock_sleep` for one tick, repeat. The tick is `1000000000/hz` ns and
`hz` defaults to **100** (`src-netbsd/sys/conf/param.c:116`) → a wake **every 10
ms**. Each tick `hardclock()` drives callouts and schedules softint threads
(`sithread`, `intr.c:158`), and the clock thread itself must re-acquire the single
rump CPU every 10 ms. So the kthread population is never allowed to settle — there
is a CPU hand-off storm 100×/second even at idle.

### 3. Every CPU release is a `cv_broadcast` — a thundering herd

When a thread releases the rump CPU it does:

```c
/* scheduler.c:492 — rump_unschedule_cpu1 */
if (rcpu->rcpu_wanted)
        rumpuser_cv_broadcast(rcpu->rcpu_cv);   // wakes ALL waiters
```

`cv_broadcast` wakes **every** thread waiting for the CPU, but only **one** can
acquire it (the CAS at `scheduler.c:339`). The other ~18 wake, lose the race, and
go straight back to `cv_wait`. On Akuma's round-robin scheduler each of those
wakeups makes the thread *runnable* and costs it a scheduler slice before it
re-blocks.

### Putting it together → the ~270 ms/hop, linear-in-threads cost

A proxied syscall needs the rump CPU several times (the doc's "~4 hops/syscall").
Each hop:

1. the heartbeat + the in-flight work generate a CPU release → `cv_broadcast`,
2. ~19 threads become runnable at once,
3. Akuma round-robins all of them; the thread doing the *actual* proxied work
   advances ~one step per full sweep of ~20 runnable threads,
4. ~270 ms later it gets the CPU, makes one hop of progress, releases → herd again.

This is exactly why **halving the thread count halved the latency** (fewer threads
in each herd = shorter sweep) and why `ps` catches the threads `running` rather than
futex-blocked: the 10 ms heartbeat re-wakes the herd faster than it can drain, so a
sample almost always lands mid-churn.

### Correction to the prior framing

`HANDOFF.md` calls the fix "make idle rump kthreads truly block (futex sleep)
instead of spin." The code shows they **already** block on real futexes — the bug
is not *failure to sleep*, it's *being woken too often and too broadly*. So the fix
is to **cut the wake frequency** (the heartbeat) and **cut the wake fan-out**
(broadcast → targeted), not to add sleeping. This is also why the two earlier
attempts failed: making `sys_ppoll` event-driven and making the *kernel proxy's*
channel read sleep both touched the **client** side, but the herd is entirely
inside `rump_server`.

---

## The plan

Three levers, in order of cost/leverage. Do them as separate, independently
measurable steps — each has a clean before/after number.

### Step 0 — Instrument to confirm the mechanism (cheap, do first)

Before changing anything, prove the herd hypothesis so we're not guessing:

- Build `rump_server` with `--features rumpuser_debug` and count, over one curl:
  `clock_sleep` calls (should be ~100/s — the heartbeat), `cv_broadcast` calls, and
  `cv_wait_nowrap` returns. Add a transient counter print to
  `rumpuser_cv_broadcast` / `rumpuser_clock_sleep` in `rumpuser/src/lib.rs`.
- Expected confirmation: broadcasts and cv_wait wakeups scale with elapsed wall
  time (heartbeat-driven), and the per-broadcast wake count ≈ `rcpu_wanted` ≈ the
  thread count.
- **Gate:** numbers match the model → proceed. They don't → re-investigate before
  touching the stack.

### Step 1 — Lower `hz` before `rump_init` (PRIMARY fix; cheap, reversible)

`hz` is a plain global (`int hz = HZ` = 100, `conf/param.c:116`) read by `doclock`
at clock-thread start (`intr.c:112`, `thetick = 1000000000/hz`). Nothing reassigns
it after boot, and the clock thread is created *inside* `rump_init`, so setting it
in `rump_server.c` **before** `rump_init()` takes effect cleanly.

- In `rumpuser/rump_server.c`, before the `rump_init()` call (`rump_server.c:112`):

  ```c
  /* Cut the rump kernel heartbeat: at ncpu==1 the default 100 Hz hardclock
   * re-wakes the whole kthread herd every 10 ms and dominates sysproxy latency.
   * (NetBSD globals; see src-netbsd/sys/conf/param.c.) */
  extern int hz, tick, tickadj;
  hz = 20;                         /* 50 ms tick — sweep candidates: 10 / 20 / 25 */
  tick = 1000000 / hz;             /* keep derived timers coherent */
  tickadj = (240000 / (60 * hz)) ? (240000 / (60 * hz)) : 1;
  ```

  (Attribution comment required — these are NetBSD globals; see
  `feedback_attribute_netbsd_source`.)

- Requires a container rebuild + relink of `rump_server`
  (`docker-build-rump-server.sh`).
- **Sweep** hz ∈ {10, 20, 25, 50, 100} and record per-syscall latency + total curl
  time for each. Expect roughly linear improvement as hz drops (fewer heartbeat
  hand-offs per second).
- **Validation (must still pass at the chosen hz):** `curl -H Host:ifconfig.me
  http://34.160.111.145` returns the IP, and the sic IRC path still registers.
  Lower `hz` coarsens TCP/callout timer resolution — watch for slower
  retransmit/DHCP, not breakage. Pick the lowest hz that keeps DHCP + TCP healthy
  (likely 10–25).
- **Risk:** DHCP at boot already has a known `--net` `ppoll` busy-loop (Open items);
  don't conflate. Test the data-plane (`--net`) path explicitly after lowering hz.

### Step 2 — `cv_broadcast` → `cv_signal` on the CPU-wait cv (SECONDARY; kills the per-hop herd)

Even at low hz, every CPU hand-off during an active syscall still broadcast-wakes
the whole herd. Only one thread can take the CPU, so waking one is sufficient:

- Patch `scheduler.c:492` (`rump_unschedule_cpu1`) `rumpuser_cv_broadcast(rcpu->rcpu_cv)`
  → `rumpuser_cv_signal(rcpu->rcpu_cv)`. (NetBSD source edit — attribution comment +
  note in the build that we patch upstream `scheduler.c`.)
- **Correctness review (mandatory — this is the risky one):** confirm the hand-off
  chain can't stall. The releaser signals one waiter; that waiter either wins the
  CPU (`scheduler.c:333` loop) or, if it can't (migration/race), must re-wait — and
  then *someone* must signal it again. Walk every exit of the `for(;;)` loop and the
  `rcpu_wanted` accounting (`scheduler.c:361`) to prove no lost-wakeup. If there's
  any doubt, keep broadcast but add a "wake at most K" cap, or only signal when
  `rcpu_wanted == 1`.
- This is independent of Step 1 and multiplies with it (lower hz × smaller herd).
- **Validation:** same curl + IRC + DHCP gates; plus a stress loop (repeated
  socket/connect/close) to flush out any hand-off stall under load.

### Step 3 — Reduce the thread count at the source (DEEP; deferred/blocked)

The herd size *is* the kthread count (~19). The structural fix is the **fiber**
rumpuser backend (`rumpfiber.c`, one OS thread per kernel, ucontext green threads),
which collapses ~19 → ~1 and removes the single-CPU contention entirely.

- **Blocked:** `rumpfiber_sp.c` stubs the sysproxy server (`abort()`), so fiber
  needs a from-scratch sp-server port (`HANDOFF.md` NEXT TASK #1,
  `RUMP_SYSPROXY.md` "Future optimizations").
- Out of scope for this pass. Steps 1+2 are expected to get latency into a usable
  range without it; revisit fiber only if they don't.

---

## Order, expected payoff, and exit criteria

1. **Step 0** (instrument) — confirm the model. ~30 min.
2. **Step 1** (lower hz) — the big, safe lever. Expect a multiple-× cut purely from
   the heartbeat. Ship this first.
3. **Step 2** (broadcast→signal) — removes the residual per-hop herd; do only after
   a careful no-lost-wakeup review.
4. **Step 3** (fiber) — only if 1+2 are insufficient; currently blocked.

**Exit criterion for this work:** per-proxied-syscall latency down from ~0.8–4 s to
the low tens of ms (no remaining herd cost), curl + sic still pass, DHCP still
leases. Plus the project-policy kernel boot self-tests for the proxy path
(`HANDOFF.md` NEXT TASK #4) — though the rump-side latency changes are validated
in-VM against the live curl/IRC paths, not host unit tests.

## Source pointers

- `src-netbsd/sys/rump/librump/rumpkern/scheduler.c:290` — `rump_schedule_cpu`
  (single-CPU acquire, the `for(;;)` + `cv_wait_nowrap` at :360).
- `src-netbsd/sys/rump/librump/rumpkern/scheduler.c:492` — the `cv_broadcast` herd
  (Step 2 target).
- `src-netbsd/sys/rump/librump/rumpkern/intr.c:117` — `doclock` 100 Hz heartbeat.
- `src-netbsd/sys/conf/param.c:116` — `hz`/`tick`/`tickadj` globals (Step 1 target).
- `rumpuser/src/lib.rs:734-800` — our cv primitives (confirmed real blocking).
- `rumpuser/rump_server.c:112` — `rump_init()` call site (Step 1 edit goes before it).
- `docs/HIJACK_VS_KERNEL_PROXY.md` — why this fix helps *every* client model, so it's
  the right place to spend effort (not relocating the sysproxy client).
