# redis-benchmark hang under host load (2026-08-24)

> ## RESOLVED later the same day — the spin is in the client, and it makes no syscalls
>
> Reproduced live with instrumentation (`-n 400000 -c 32` against a
> `net-profile` guest) and root-caused. **The guest is not involved in the
> hang at all**, and the mechanism is a `while(1)` in `redis-benchmark`'s own
> `writeHandler`.
>
> Evidence gathered during a live wedge:
>
> | observation | reading |
> |---|---|
> | client 100 % CPU, QEMU 5-8 % | client spinning, guest idle |
> | `redis-cli ping` → instant `PONG`; ssh fine; guest load average 0.00 | **the guest is healthy throughout** |
> | `[NICSTAT]` windows 22→46 contiguous at `dt=5000ms` | the netpoll loop never stalled |
> | every client socket `Recv-Q=7`, `Send-Q=0` | `+PONG\r\n` was delivered and sits **unread**; nothing is blocked on write |
> | lldb: breakpoints on `write` **and** `sendto` resolved, **hit count 0** over 35 s of spinning | **zero syscalls** — a pure userspace loop |
> | stack pinned at `writeHandler` → `hi_sdscatlen` / `hi_sdsMakeRoomFor` / `memmove` | re-appending the request buffer, unbounded |
>
> **Mechanism.** `writeHandler` wraps its send in a `while(1)` "optimistically
> try to write" retry. When `cliWriteConn` returns neither `writeLen`, nor
> `> 0`, nor `-1` with a non-`EAGAIN` errno, **no branch returns** — so it
> loops, re-appending the command to `obuf` each pass. A single transient
> `EAGAIN` (Akuma applying backpressure at its round-trip ceiling) becomes a
> permanent livelock that outlives the condition that caused it. `Send-Q=0`
> proves that directly: the backpressure cleared long ago and the client never
> recovered.
>
> **This resolves every open item below.**
>
> - The reviewer was right that the absence of `hi_sdsrange` disproved a
>   partial-write retry — **because there is no write at all.**
> - Open item 1 ("trace actual `write()`/`read()` return values … `dtruss`
>   needs interactive sudo") is answered without sudo: lldb breakpoints on
>   `write`/`sendto` never fire.
> - Open item 2 (packet capture) is moot: `Recv-Q`/`Send-Q` show the wire is
>   idle and the reply already arrived.
> - Open item 3 ("is the trigger host contention as a category?") — host load
>   is a *trigger*, not the mechanism. Anything that makes one write return
>   `EAGAIN` will do it.
> - Why the raw-socket probe never hung: **blocking sockets never see
>   `EAGAIN`.** Why Docker never hung: Linux is fast enough here that the send
>   buffer never fills.
> - Open item 4 (the "~2.5x regression", ~20,000 → ~8,000 rps) is answered in
>   [`REDIS_ROUND_TRIP_CEILING.md`](REDIS_ROUND_TRIP_CEILING.md) §5: it is
>   degradation with **VM uptime**, not code. Fresh boot 14,085 rps; the
>   long-running VM that produced the ~8,000 figure, 7,634.
>
> **Practical consequence unchanged, reason changed:** "client near 100 % CPU
> with QEMU idle" still means kill and retry — but it is a client bug being
> tripped, not a guest hang, so nothing in the guest needs recovering.
>
> The round-trip ceiling that produces the backpressure is the real Akuma-side
> finding, and it has its own document:
> [`REDIS_ROUND_TRIP_CEILING.md`](REDIS_ROUND_TRIP_CEILING.md).

**Status (original): OPEN. Confirmed real, confirmed Akuma-specific, mechanism NOT
identified.** The first writeup of this investigation over-claimed a root
cause; an independent review (clean-context opus agent) caught that the
evidence didn't actually support the stated mechanism, and two follow-up
experiments then disproved that mechanism outright. This version reflects
where the investigation actually stands. Filed while re-running
`BENCHMARK_PERFORMANCE_ATTEMPT_0.md`'s redis matrix
(`docs/archive/BENCHMARK_PERFOMANCE_ATTEMPT_1.md`), because the very first arm
(`akuma-fwd`, SMP=4, `-n 100000 -c 20 -P 1 -t ping`) hung for 6-13+ minutes
burning 98-100% of one host core while QEMU sat at single-digit percent — a
test that historically completes in ~5 seconds
(`BENCHMARK_PERFORMANCE_ATTEMPT_0.md` §3: ~20,000 ops/s).

## Symptom

`redis-benchmark` (the host-side client, hitting the guest's forwarded port
over QEMU SLIRP) pegs one core at ~100% CPU and makes almost no forward
progress, while `qemu-system-aarch64` sits at single digits. The guest's own
redis-server is not hung: a manual `redis-cli ping` against it succeeds
instantly throughout.

## What it isn't

- **Not a stale/orphaned process measuring the wrong thing.** Reproduced with a
  freshly rebooted guest, a single verified `redis-benchmark` process, correct
  disk image, correct core count.
- **Not the documented socket-budget ceiling** (`MAX_BACKLOG`,
  `docs/archive/AKUMA_NET_ISSUES.md` §11.2) — that failure mode is
  `redis-benchmark` printing `No file descriptors available` and exiting 0
  with missing result rows, not a hang with no exit.
- **Not a fixed request-count threshold.** `-n 100000 -c 20 -d 64 -P 1 -t ping
  --csv` — the literal harness command — completes cleanly in ~25s (~8,000
  rps) when run in isolation, as does `-n 10000` / `30000` / `60000`. The hang
  needs something else present that isn't present in an isolated run.
- **Not generic host-CPU-contention-does-this-to-any-virtualized-guest**, and
  **not a client-side partial-write/backpressure spin** — both ruled out below
  by direct experiment, after having been the (wrong) headline claim of the
  first version of this document.

## Timeline: a wrong conclusion, caught and then disproven

**First hypothesis (retracted):** host CPU contention starves QEMU's vCPU
threads → the guest's netpoll loop falls behind draining the TCP connection →
`write()` on the host side returns partial counts → hiredis's `writeHandler`
has to repeatedly shift its pending-output buffer, an O(n)-ish cost that
itself burns host CPU → self-reinforcing livelock. This was written up as
"root-caused, not an Akuma kernel bug" based on: (a) `sample`-ing the stuck
process showing CPU inside hiredis's `hi_sdscatlen`/`hi_sdsMakeRoomFor`/
`hi_sdsclear`, (b) the guest's own `PSTATS` showing a low read/write count,
(c) every isolated run succeeding, and (d) deliberately saturating the host
with six `yes` processes reproducing the identical stuck signature.

**Independent review caught real holes before any further work was done:**

1. The symbol that would actually indicate a partial-write retry path —
   `hi_sdsrange`, the memmove that shifts an unsent remainder — was *absent*
   from the profile. The symbols present (`hi_sdscatlen`, `hi_sdsMakeRoomFor`,
   `hi_sdsclear`) are the *append a new command* / *buffer fully drained*
   paths. At `-P 1`, each client has one small outstanding command at a time —
   there is no large pending buffer for an O(n) reshuffle to be expensive
   against. The claimed mechanism didn't match its own evidence.
2. QEMU at single-digit CPU is evidence *against* vCPU starvation, not for it:
   a starved-but-runnable process fights for its share and shows up *busy*
   when sampled, not idle. Single digits means QEMU had little to do, i.e.
   packets weren't arriving — pointing at the SLIRP path or connection state,
   not scheduler contention.
3. ~71k reads over the observed window at `-P 1` is roughly one read per
   request — that's normal framing and real forward progress, not "almost no
   progress" as originally characterized.
4. The `yes`-load reproduction had no control: it was never run against the
   Docker arm with the same stress, so "the bug lives in the client, any
   guest would show this under contention" was asserted, not tested.

**Two follow-up experiments, run after the review:**

- **Docker control** (`docker exec akuma-redis-bench`, port 6379): the exact
  same `-n 100000 -c 20 -d 64 -P 1 -t ping --csv` under the exact same six-`yes`
  host stress completed **cleanly at 34,246 rps — no hang.** Docker Desktop is
  also virtualized (Apple's Virtualization.framework, not bare metal), so "any
  hypervisor guest degrades like this under host contention" is now directly
  falsified. This is specific to Akuma (or to Akuma's networking path via QEMU
  SLIRP specifically — Docker's networking backend differs, so the comparison
  isolates *something* about that pairing, not necessarily "the Akuma kernel"
  alone).
- **Raw-socket ground truth** (`scripts/probes/redis_write_probe.py` — 20
  threads, blocking sockets, same PING flood, no hiredis event loop involved):
  against Akuma port 4444, under the identical six-`yes` stress, this
  completed in 8.15s with **zero partial writes** and a modest, graceful
  slowdown (7,863 → 4,907 rps — real contention cost, not a livelock). This
  directly disproves the partial-write mechanism: a client doing the same
  request pattern against the same guest under the same load does not hang,
  and never sees a short `send()`.

## Where this leaves the investigation

Two things are now solid:

- **The hang is Akuma-specific**, not a generic virtualization-under-load
  artifact (Docker control).
- **It is not a partial-write/backpressure spin**, and does not require a
  degraded guest netpoll loop to reproduce against a well-behaved client (raw
  socket probe).

What's left unexplained: **`redis-benchmark` itself, and specifically its own
event loop (hiredis + the bundled `ae.c`), is required to trigger this against
Akuma under load — a plain blocking-socket client is not enough.** The
mechanism by which `redis-benchmark`'s non-blocking, event-driven connection
handling interacts badly with something Akuma-side (packet timing, TCP option
negotiation, `epoll`/readiness semantics as seen through QEMU SLIRP, or
something else entirely) is not identified. Concretely still open:

1. Trace actual `write()`/`read()` return values *from redis-benchmark itself*
   (not a substitute client) during a hang — `dtruss`/`ktrace` needs
   interactive sudo, not available in this session; worth doing by hand.
2. Packet capture (`tcpdump` on the loopback/forwarded port) during a live
   hang — also blocked here by requiring an interactive sudo password: worth
   doing by hand to see window/ACK/retransmit behavior at the wire level.
3. Whether the trigger is really "host contention" as a category, or
   specifically QEMU SLIRP's own single-threaded packet handling being more
   sensitive to scheduling delay than Docker's networking backend — the `yes`
   experiment establishes correlation with host load, not which component's
   sensitivity to that load is the proximate cause.
4. The ~8,000 rps ceiling seen in every *clean* isolated run, against
   attempt 0's ~20,000-20,794 rps for the identical arm, is a real, separate,
   currently-unexplained ~2.5x regression that this investigation surfaced but
   did not chase — worth its own look before being dismissed as environment
   noise.

## Practical implication for the benchmark matrix

Whatever the exact mechanism, it is real and reproducible under host load, so
until root-caused: don't run other CPU-heavy work on the host while a
`redis-benchmark`-driven arm is in flight, and treat "client near 100% CPU
with QEMU idle for more than a few seconds" as a signal to kill and retry
immediately rather than wait.

## Background

- [`BENCHMARK_PERFORMANCE_ATTEMPT_0.md`](BENCHMARK_PERFORMANCE_ATTEMPT_0.md) —
  the original redis matrix and the numbers this session was trying to
  reproduce.
- [`BENCHMARK_PERFOMANCE_ATTEMPT_1.md`](BENCHMARK_PERFOMANCE_ATTEMPT_1.md) —
  the re-run this investigation blocked.
- `scripts/probes/redis_write_probe.py` — the raw-socket ground-truth probe
  used above; reusable for any future partial-write question against any
  guest port.
- `src/main.rs:1444`, `src/timer.rs:50-56` — the netpoll loop and tick
  cadence, the (now-disproven) mechanism's original suspect.
