# nginx's 17 ms request: a lost epoll wakeup rescued by the backstop (2026-08-25)

**Status: hypothesis with strong circumstantial support, NOT proven.** Nobody
has yet traced an actual missed wake. §5 says exactly what would settle it.
Do not act on this by tuning `backstop_us` — see §4.

> **The one-line answer.** In its slow regime nginx serves a request in
> **17.8 ms**, and `ab -k` (connection reuse) halves that to **8.6 ms** — which
> sits just under `BLOCKING_POLL_INTERVAL_US` = **10 ms**, the epoll family's
> `backstop_us`. nginx is not computing slowly. Its `epoll_wait` is **missing
> the readiness wake**, and the backstop timer is what ends the wait — about
> twice per non-keep-alive request, once with keep-alive.

---

## 1. The measurement

Guest in the slow regime, `ab -n 100` in-guest against nginx on `/public`,
`SMP=4`, on the trace-gated kernel:

| arm | rps | ms/req | complete | failed |
|---|---:|---:|---:|---:|
| nginx `-c 1` | 56 | **17.8** | 100 | 0 |
| nginx `-c 4` | 134 | 29.7 | 100 | 0 |
| nginx `-c 1 -k` (keep-alive) | 116 | **8.6** | 100 | 0 |

**Zero failed requests.** Nothing is erroring or retrying; every request
completes correctly, just late.

## 2. Why this reads as the backstop

- **8.6 ms is just under 10 ms**, and 10 ms is exactly
  `BLOCKING_POLL_INTERVAL_US` (`src/syscall/poll.rs:55`), which
  `effective_poll_interval_us()` hands to `WaitPolicy::epoll` as `backstop_us`
  (`crates/akuma-net-yarn/src/lib.rs`). A wait that ends on the backstop ends
  at *most* one backstop period late, so a mean just under the constant is the
  signature of "essentially every wait rode the timer".
- **Keep-alive exactly halves it** (17.8 → 8.6). A non-keep-alive request needs
  one more readiness transition than a reused connection does — the accept /
  new-connection readiness on top of the request-readable one. Two stalls
  become one. A cost that scales with the *number of readiness waits* rather
  than with bytes, connections, or CPU is a per-wait constant, not work.
- **`-c 4` does not fix it** (29.7 ms/req at 4x concurrency = 134 rps vs 56).
  Throughput rises only ~2.4x while per-request latency nearly doubles, i.e.
  the stalls overlap imperfectly rather than disappearing — consistent with a
  fixed timer each connection waits on independently.

This is the same *shape* as the `sys_pselect6` bug in
[`LONG_ROAD_TO_REDIS_PART_2.md`](LONG_ROAD_TO_REDIS_PART_2.md) §2a — a wait
that can only end on the tick — but the mechanism differs: there the waker was
never registered, here it is registered and the wake is **lost**.

## 3. It explains the bimodality nothing else does

The same nginx, same kernel, same docroot, measured across four boots:

| boot | µs/req |
|---|---:|
| A | 114 |
| B | 308 |
| C | 513 |
| D | **17,800** |

**150x spread, but tight *within* any one boot** (D read 17.2 / 17.0 / 17.5 ms
across three runs). That is a bimodal system state, not measurement noise: a
boot either lands where these wakes are delivered or where they are not.

**Redis never does this.** It reads 41 µs (UNIX socket) / 114 µs (TCP loopback)
on every healthy boot, reproducibly, and does not move after 2,400 connections.
So whatever is lost is specific to nginx's epoll usage rather than to epoll or
the scheduler generally. Candidates, none yet checked:

- nginx re-arms interest per request (`epoll_ctl MOD`) around partial writes;
  a wake delivered between the readiness scan and the park would be lost if the
  re-arm resets the registration.
- `EPOLLOUT` interest toggling — the delayed-first-byte work already found one
  "unarmed EPOLLOUT edge" bug in this family (see Background).
- The listening-socket path specifically, since keep-alive (which removes one
  accept per request) removes exactly one stall.

## 3a. Akuma's own `httpd` is a natural control — it uses no epoll

`userspace/httpd` contains **no `epoll` and no `poll` call at all** (grep over
`userspace/httpd/src/*.rs` returns nothing): it is a single-threaded blocking
`accept` / `read` / `write` loop. That makes it the control this investigation
would otherwise have had to build.

Measured on the **same boot**, same tool (`ab -c 1`), same docroot, while that
boot was in the slow regime:

| server | I/O model | rps | ms/req |
|---|---|---:|---:|
| Akuma `httpd` | blocking | **909** | **1.10** |
| Akuma nginx | epoll | **56** | **17.8** |

**On a boot where nginx costs 17.8 ms, the blocking server on the same kernel
costs 1.10 ms — 16x faster.** A kernel that was simply slow at serving files,
or at TCP, or at scheduling, would penalise both. Only the epoll-driven one is
affected, and only on some boots.

Together with redis (which uses epoll but never shows the bimodality) this
narrows the suspect considerably: it is not epoll in general, but something
about **nginx's particular use of it** — see the candidate list in §3.

## 4. What this reframes — do NOT tune the backstop

[`LONG_ROAD_TO_REDIS_PART_2.md`](LONG_ROAD_TO_REDIS_PART_2.md) §8 A/B'd
`backstop_us` 10 ms → 1 ms and found sweep `c=1` improved ~10 % while `c=32`
lost 5 %. In light of this, that result reads differently: lowering the
backstop **shortens the rescue, it does not fix the miss**. The `c=1` gain is a
measure of how often the wake is lost, not a scheduling improvement — and the
`c=32` regression is the price of waking every parked poller ten times as often
for no reason.

**Fix the lost wakeup. Leave `backstop_us` at 10 ms** until it is fixed, then
re-A/B it against a kernel that no longer needs rescuing.

## 5. How to settle it

Two ways, either sufficient:

1. **Trace it.** Boot into the slow regime, turn on
   `crate::config::SYSCALL_DEBUG_NET_ENABLED`, and check whether
   `sys_epoll_pwait` returns are landing on the park deadline rather than on a
   waker. `log_epoll_pwait_return` already records `iterations` and elapsed;
   a wait that rode the backstop shows elapsed ≈ 10 ms with the readiness
   appearing only on the final lap. **Note the trap**: that flag also re-enables
   the `epoll_ctl` trace whose *ungated* version was 99.3 % of console output
   and inflated latency 7.4x
   ([`LONG_ROAD_TO_REDIS_PART_2.md`](LONG_ROAD_TO_REDIS_PART_2.md) §9) — so the
   traced run measures nothing, it only tells you *which* return path was taken.
2. **A/B the constant.** Build with `BLOCKING_POLL_INTERVAL_US = 1_000` and
   re-measure a slow-regime boot. If 17.8 ms becomes ~1.8 ms, it is the
   backstop by construction. This is a diagnostic, not a fix (§4).

The hard part is **getting into the slow regime on purpose** — it is a
per-boot state and nobody knows what selects it. Boot repeatedly, measure nginx
with `ab -n 100 -c 1`, and keep the first boot that reads >5 ms. Take the redis
UNIX/TCP-loopback reading (41/114 µs) on the same boot to confirm the guest is
otherwise healthy — that separates "this boot is degraded at everything" from
"this boot loses nginx's wakes specifically", which is the whole question.

## 6. Reproducing the measurement

`ab` is **not** in the devbox image: `apk add apache2-utils`. A `/bin/curl`
shell loop is not a substitute — it puts client cost inside the measurement.
Start **one** server per boot, and validate: `curl -w '%{time_total}'` reports
a time for a *connection-refused* request too, so a dead server measures as
extremely fast (this produced a phantom 6.8x result earlier the same session).
Check `Complete requests` / `Failed requests` in `ab`'s own output.

nginx in the devbox needs `user root;` (there is no `nginx` user —
`getpwnam("nginx") failed`) and `sendfile off;` (`sendfile(2)` is not
implemented — see `../runbooks/run-docker-image.md`).

## Background

- [`LONG_ROAD_TO_REDIS_PART_2.md`](LONG_ROAD_TO_REDIS_PART_2.md) — §8 the
  backstop A/B this reframes, §9 the ungated-trace contamination and the
  measurement rules, §9e the first writeup of this finding.
- [`BENCHMARK_PERFOMANCE_ATTEMPT_1.md`](BENCHMARK_PERFOMANCE_ATTEMPT_1.md) §6 —
  the nginx/httpd numbers and the harness requirements.
- [`SOCKET_DELAYED_FIRST_BYTE_HANG.md`](SOCKET_DELAYED_FIRST_BYTE_HANG.md) — the
  earlier lost-wakeup cohort in this subsystem, including an unarmed
  `EPOLLOUT` edge.
- [`../reference/subsystems/syscalls/poll.md`](../reference/subsystems/syscalls/poll.md)
  § "The wait loop is one machine" — where `backstop_us` lives and what else
  differs between the two wait families.
