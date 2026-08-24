# The Redis round-trip ceiling — located, quantified, and half of it removed (2026-08-24)

**Status: root-caused and A/B-confirmed.** `BENCHMARK_PERFORMANCE_ATTEMPT_0.md`
§4 found "a fixed round-trip ceiling" and could not say what set it.
`AKUMA_NET_ISSUES.md` §12 named `tx_wait` as "the largest named cost left" but
measured it with a serial client that could never reach the ceiling. This
document finds the ceiling, gives it an equation that predicts its value at
three core counts, and removes 43 % of it with a two-line change.

> **The one-line answer.** Akuma's Redis throughput is
> `1 / (microseconds spent inside smoltcp_net::poll() per round trip)`.
> `poll()` runs under the global `NETWORK` spinlock inside a `PreemptGuard`
> that masks IRQs, so exactly one core can be in it at a time. Cores and
> clients cannot raise that number; only making `poll()` cheaper can.

---

## 1. The measurement that changes the picture: sweep concurrency

Every prior Redis number in `docs/archive/` was taken at a **single** client
count (`-c 20`), and every prior HTTP number came from `bench_nic_rtt.py`,
which is **serial** — `AKUMA_NET_ISSUES.md` §11 item 4 lists "a concurrent-
connection harness" as *not done*. That gap is why the ceiling was never seen:
a serial client never reaches it, and one client count cannot tell "slow" from
"serialized".

`scripts/benchmarks/redis_conc_sweep.py` sweeps it. PING, `-P 1`, `-d 64`,
median of 3, fresh boot per arm, Docker control on the same host in the same
session:

| clients | SMP=1 | SMP=2 | SMP=4 | `main` SMP=4 | Docker |
|---:|---:|---:|---:|---:|---:|
| 1 | 449 | 1,380 | 1,321 | 1,326 | 8,734 |
| 2 | 2,498 | 2,437 | 2,646 | — | 15,699 |
| 4 | 4,824 | 11,312 | 12,723 | 12,376 | 22,422 |
| 8 | 6,353 | 12,210 | 13,441 | — | 36,101 |
| 16 | 7,413 | 12,531 | 13,831 | 14,085 | 51,546 |
| 32 | 6,068 | 12,407 | **14,085** | 13,850 | **64,516** |

**Akuma plateaus from four clients on; Docker is still climbing at 32.** The
often-quoted "~4x slower than Docker" is *entirely* this plateau: the ratio is
1.8x at four clients and 4.6x at thirty-two, growing with every client added.
It is not a per-round-trip cost — at one client Akuma is 6.6x behind, at four
it is 1.8x behind, and the number you get depends only on where you stop
sweeping.

That shape — throughput flat while per-client latency rises exactly linearly —
is a serially-drained resource, not a slow one. A merely slow stack keeps
scaling until it saturates something.

## 2. The ceiling has an equation

`net-profile` gives `[NICSTAT]` a per-window time budget. Taking only windows
with a saturating workload in them, and dividing the time spent inside
`smoltcp_net::poll()` by the round trips completed:

| | µs in `poll()` per round trip | `1/that` | measured plateau |
|---|---:|---:|---:|
| SMP=1 | 165.0 | 6,060 | 7,413 |
| SMP=2 | 82.9 | 12,062 | 12,531 |
| SMP=4 | 69.3 | 14,430 | **14,085** |

Three independent core counts; the SMP=4 prediction is within **2 %**. This is
the ceiling, and `poll()` is where it lives.

**Why it is a hard ceiling.** `poll()` is called through `with_network()`,
which takes the single global `NETWORK` `Spinlock`, wrapped in a `PreemptGuard`
that masks local IRQs. One core at a time, always. Total system throughput is
therefore `1 / (serialized poll time per round trip)` no matter how many cores
are idle or how many clients are waiting.

**Why cores stop helping at two.** There are only two actors — the async-main
drain loop and single-threaded `redis-server`. One core makes them timeshare;
two lets them pipeline, and the per-round-trip poll cost halves (165 → 83 µs).
Past two, extra cores cannot enter the lock: 2 → 4 buys 17 %.

### What the 69.3 µs is made of (SMP=4)

Nesting per `nicstat_breakdown.py`: `tx_wait` and `rx_post` happen *inside*
`poll()`, so they are shares of it, never added alongside.

| component | µs/round trip | share |
|---|---:|---:|
| `tx_wait` — `add_notify_wait_pop` busy-spin | 29.7 | 43 % |
| smoltcp stack — `iface.poll()` socket walk | 34.8 | 50 % |
| `rx_post` — one MMIO notify per packet | 4.8 | 7 % |

with **3.33 `poll()` calls** and **1.96 TX packets** per round trip.

- **`tx_wait`** is `VirtIONetRaw::send`, i.e. `add() → notify() → while
  !can_pop() { spin_loop() } → pop_used()`. It busy-waits for QEMU's SLIRP
  thread to consume the descriptor, with `NETWORK` held and IRQs masked
  (`crates/akuma-net/src/virtio_rings.rs` header, `AKUMA_NET_ISSUES.md` §3.2).
  ~14.9 µs per packet, paid **twice** — see §3.
- **The smoltcp half is partly the socket-table walk — but less of it than this
  number suggests.** `iface.poll()` walks the whole `SocketSet` every call, and
  §11.1 measured 10.6 µs/call at 128 slots against this run's 10.7 µs/call at
  85. That agreement is what made the walk look dominant. **§6 later disproves
  that reading**: removing one of the three `poll()` calls per round trip left
  total poll time unchanged and simply raised µs-per-call, so most of this term
  is per-round-trip TCP work that has to happen in *some* call, not a fixed
  per-call tax. Treat the split as undecomposed.
- **3.33 calls per round trip** is not the broadcast wake (§4). §11.5 already
  explains it: `socket_send` and `socket_recv` each poll on the way out, on top
  of epoll's own poll.

## 3. The duplicate ACK — found, removed, +43 %

Every `[NICSTAT]` window on a request/response workload shows **1.97-2.00 TX
packets per RX packet**, at every core count and on both branches. Akuma emits
a bare ACK *and* the response for each request.

The cause is one line, `crates/akuma-net/src/smoltcp_net.rs`:

```rust
// Disable delayed ACK so receive-heavy workloads aren't throttled
// to ~65KB/10ms by piggyback waiting.
socket.set_ack_delay(None);
```

With `None`, smoltcp ACKs the request the instant it lands — before the server
has produced a reply — so the reply cannot piggyback. Each duplicate costs a
full `add_notify_wait_pop` spin inside the lock.

**The comment's justification does not hold for smoltcp 0.12.** Delayed ACK
there is not a blanket delay: `immediate_ack_to_transmit()` forces an ACK once
one full MSS of unacked data has arrived (explicitly following the Linux
kernel's empirical rule), and `window_to_update()` forces one whenever the
receive window doubles. Bulk receive still ACKs per segment. Only the sub-MSS
request/response case waits — exactly the case that wants to piggyback — and
the timer never expires there, because Redis replies in ~100 µs, far inside it.

Changing it to `Some(Duration::from_millis(10))`:

| | baseline | delayed ACK | |
|---|---:|---:|---|
| TX packets per round trip | 1.96 | **0.98** | halved exactly |
| µs in `poll()` per round trip | 69.3 | **43.7** | |
| ├ `tx_wait` | 29.7 | 17.8 | |
| └ smoltcp stack | 34.8 | 21.6 | fewer packets to dispatch |
| predicted ceiling | 14,422 | 22,882 | |
| **measured, c=32** | **14,085** | **20,202** | **+43 %** |
| latency at c=1 | 746.8 µs | **391.7 µs** | **−48 %** |

The model predicted the new ceiling before it was measured, from the packet
count alone. That is the strongest evidence that §2's equation is the real
mechanism and not a coincidence.

### The trade is real, and smaller than the comment claimed

The old comment deserved a measurement, not just a code reading, so
`scripts/benchmarks/redis_bulk_ab.py` ran the contested cell with repeats on
both kernels in one session:

| cell | `None` | `10ms` | verdict |
|---|---:|---:|---|
| SET `-d 65536` (guest **receives**) | 917.7 (spread 0.6 %) | 745.8 (spread 0.7 %) | **−18.7 %, real** |
| GET `-d 65536` (guest sends) | 337 | 315 | −6.5 %, inside 10.3 % spread |
| SET `-d 4096` | 8,185 | 10,299 | +26 % |
| GET `-d 4096` | 7,559 | 8,258 | +9 % |

So delayed ACK **does** cost receive-heavy bulk, but **~19 %, not the
catastrophe the comment predicted** — 48.9 MB/s, nowhere near the "~65KB/10ms"
(6.5 MB/s) it warned of. Three of four bulk cells improve.

**Shortening the delay does not buy the trade back.** `1ms` was tried: 19,417
rps at c=32 (vs 20,202 at `10ms`, 14,085 at `None`) and 776 ops/s on 64 KB SET
(vs 746 and 918). The bulk cost is therefore **structural to ACKing less
often, not a function of the timer length** — do not tune the constant hoping
to escape it. `10ms` is kept because it gives the better round-trip number and
the bulk difference between the two is small.

**Net:** +43 % on the round-trip ceiling and −48 % on single-client latency,
against −19 % on 64 KB receive-heavy bulk. Good trade for a Redis/HTTP/RPC
workload; if a future workload is dominated by large inbound writes, this is
the knob to revisit — with `redis_bulk_ab.py`, not by reasoning.

## 4. Three things that are NOT the ceiling

Each was a live hypothesis; each was tested and is recorded so it is not
re-tried blind.

- **The cross-core broadcast wake is not it.** §11.4 measured the NIC handler's
  broadcast SGI at 2.5 async-main laps per NIC interrupt. Under this workload
  `laps per NIC irq` is **0.21-0.27 at every core count**, and `poll()` calls
  per round trip are flat at **3.33 / 3.55 / 3.33** for SMP=1/2/4. If waking
  every core were multiplying work as cores are added, both numbers would climb
  with the core count. They do not. The broadcast is real waste; it is not what
  sets the ceiling, which is fixed before any wake happens by how long one core
  holds `NETWORK`.
- **Redis persistence is not it.** `--save ''` (what every benchmark in
  `docs/archive/` used) against a stock `--save '900 1'`: 14,085 vs 14,265 at
  c=32, 12,723 vs 12,723 at c=4. No effect, as expected for a workload that
  never dirties a key.
- **The branch is not it.** `main` (`351a8722`) and `more-fixes` (`3b38cc2a`)
  are indistinguishable — 13,850 vs 14,085 at c=32 — and both show the same
  1.945 TX-per-RX ratio. The duplicate ACK predates both.

## 5. The "2.5x regression" was VM state, not code

`REDIS_BENCHMARK_HOST_CONTENTION_LIVELOCK.md` closed by flagging a real,
unexplained gap: attempt 0 measured ~20,000 rps on this arm, and the attempt-1
re-run got ~8,000. Both branches now measure **14,085** on a **fresh boot**.

The first sweep in this session, run against the **long-running VM** that
attempt 1 had left up (hours of uptime, nginx also resident, a failed benchmark
behind it), plateaued at **7,634** — matching attempt 1. The same code, freshly
booted, does 14,085.

**So the ~1.8x is degradation with VM uptime / prior load, not a code
regression.** That is its own open bug and it is not chased here. It matters
for method: a benchmark on a VM that has already served a workload measures the
VM's history as much as the kernel. Boot fresh per arm — `redis_smp_sweep.py`
does.

## 5b. Where the socket-table half of the budget comes from: listener pools

§2 says half the per-round-trip budget is `iface.poll()` walking the whole
`SocketSet`, at 10.7 µs per call over **85 live sockets**. On a guest running
only sshd and redis, 85 is a startling number — and it is not connections.

**A listener in Akuma is not one socket. It is a pool of `MAX_BACKLOG`
pre-`listen()`ed smoltcp sockets** (`crates/akuma-net/src/socket.rs:1047`), and
`MAX_BACKLOG` is **32** under `many-sessions`. The pool exists because smoltcp
has no SYN queue: a pool entry leaves `Listen` the moment a SYN lands, so the
depth of the pool *is* the accept backlog (`NGINX_MISSING_SYSCALLS.md` Issue
E1). Two listeners therefore cost 64 table entries before a single client
connects, and `iface.poll()` walks every one of them on every call — 3.3 calls
per round trip, ~280 socket visits per PING, all inside the `NETWORK` lock,
almost all of them entries that never have anything to say.

That is the mechanism behind the 50 % smoltcp share, and it explains the
otherwise-odd `sockets=66/128` on an idle guest.

**The experiment to confirm it by growing the table hits a wall, and the wall
is itself a finding.** Adding four idle `nc` listeners (+128 pool entries) to a
66-entry table against `SOCKET_SOFT_CAP` = 128 drove the table straight into
§11.2's cliff: `sockets=` climbed 88 → 90 → 95, the generator started taking
`ECONNRESET`, and the guest stopped answering ssh. So the table-size lever can
only be pushed by **one** listener before the cap converts the experiment into
an outage. §11.2's "move reclamation off the accept path" is still not done,
and this is what it looks like from the outside.

The lever that *is* available is the other multiplier: **3.3 `poll()` calls per
round trip**. Each one pays the full walk, so halving the calls halves this
half of the budget without touching `MAX_BACKLOG` or the cap.

## 5c. Method: `redis-benchmark` cannot measure this system

It livelocks at the ceiling — the client-side `while(1)` of
[`REDIS_BENCHMARK_HOST_CONTENTION_LIVELOCK.md`](REDIS_BENCHMARK_HOST_CONTENTION_LIVELOCK.md),
tripped by exactly the backpressure that saturation produces. It killed the
socket-table sweep above on its first attempt and burned a 400,000-request run
earlier the same day.

`scripts/benchmarks/rtt_load.py` replaces it: blocking sockets (which never see
`EAGAIN`, so the client bug is structurally unreachable), **processes not
threads**, because a threaded Python client silently caps the measurement and
looks exactly like a guest ceiling. That failure is worth stating as a number —
against the same Docker endpoint the generator reads:

| processes | 8 | 16 | 24 | 32 |
|---|---:|---:|---:|---:|
| rps | 37,649 | 50,672 | 59,526 | **67,133** |

`redis-benchmark` gets 64,516 on that endpoint, so only the 32-process
configuration is measuring the server rather than the client. **Any harness
here must publish this self-test**, or its "ceiling" may be its own.

**The parity target is therefore ~64,500 rps, ~15.5 µs per round trip.**

## 5d. The floor: one MMIO notify per packet, and it is ~19 µs

The remaining 43 % of the budget is `tx_wait`, and it is **not** what
`AKUMA_NET_ISSUES.md` §3.2 says it is. §3.2 attributes it to `send()` blocking
"while it waits for the host's QEMU thread to be scheduled". Two measurements
say the waiting is not the cost:

| `emit_frame` path | what it does | µs/pkt |
|---|---|---:|
| `not(net-noalloc)` (`smoltcp_net.rs:664`) | `send()` = add + notify + **spin until consumed** | 19.5 |
| `net-noalloc` (`smoltcp_net.rs:631`) | `submit()` = ring insert, **no spin**, + one MMIO notify | 19.9 |

**Deleting the spin entirely changed nothing.** So essentially the whole ~19 µs
is the MMIO write to `QueueNotify` — a vmexit in which QEMU runs the virtio-net
TX path, and with SLIRP that means the NAT and the host-socket write, *inline*,
before control returns to the guest. The notify is not a hint that costs a
vmexit; it is the packet being transmitted synchronously.

Three properties confirm it is a fixed per-descriptor cost:

- **Byte-independent.** 16.4-17.1 µs/pkt at 61 B/pkt (PING round trips) and at
  1288 B/pkt (a 116 MB bulk transfer). 21x the payload, same cost.
- **Does not amortise.** A 92,803-packet burst pays it on every packet.
- **Unaffected by the rings**, which is the whole of §7/§12's "neutral" verdict
  explained: the rings remove a wait that was never the cost, while adding ring
  bookkeeping (smoltcp stack 22.3 → 29.9 µs/rt), `tx_stall=707`, and — because
  `receive_begin`'s per-packet notify doubled as a wake source — dropping NIC
  interrupts per round trip from 1.07 to 0.67. Re-measured here with the
  concurrent harness at SMP=4: **−14.6 % at c=32, −18.5 % at c=8, −22.5 % at
  c=1.** §12's verdict stands, now with a mechanism.

**Do not try to suppress the notify.** `transmit_begin` already honours
`should_notify()` (virtio-drivers 0.13 `queue.rs:322`), and `TxRing::submit`
deliberately kicks on top of it. That was measured on 2026-08-19 and the
rationale is in `smoltcp_net::nic_kick_tx`'s doc comment: QEMU negotiates
`VIRTIO_F_EVENT_IDX`, so a suppressed notify leaves the frame sitting in the
avail ring — **90.9 µs average submit→completion, 6,486 µs worst case, HTTP p99
6,747 µs**. The blocking path gets away with honouring suppression only because
its spin waits the suppression out. Re-tried and reverted 2026-08-24.

### What this means for parity with Docker

Docker's **entire** round trip on this host is 15.5 µs. Akuma pays 16-19 µs to
hand **one** packet to QEMU. After §3 there is exactly one packet per round
trip, so that cost cannot be divided further, and it alone exceeds Docker's
whole budget.

**Parity is therefore not reachable on virtio-mmio + QEMU SLIRP, at any amount
of kernel optimisation.** Driving every kernel-side cost to zero leaves ~19 µs,
or ~52,000 rps, and only if that cost overlapped instead of serialising.

This is not "virtualisation is slow" — Docker is virtualised too. Docker
Desktop's backend is **gvisor** (confirmed from its process arguments:
`--networkType gvisor`), a userspace netstack in the same process as the
forwarder, with no per-packet vmexit. The comparison has never controlled for
the transport, and that is where the remaining gap lives.

The lever is therefore transport, not kernel: this QEMU build offers
`vmnet-host` / `vmnet-shared` / `vmnet-bridged` (macOS `vmnet.framework`,
kernel-level, needs root) and `tap`/`vhost-user` alongside `user`. The honest
control that has never been run is **Linux under the same QEMU + SLIRP +
`hostfwd`**: if it also lands near 20,000 rps, Akuma is already at transport
parity and no kernel gap remains to close.

## 6. What to fix next, in value order

All four are independent and multiply. The budget they attack is §2's 69.3 µs
(43.7 µs with §3 landed).

Two entries that were on this list are now **struck out by measurement**, and
both were my own predictions. They are kept so nobody re-derives them.

- ~~**Stop polling on every socket-syscall exit.**~~ Predicted ~24 µs, the
  largest item. Dropping the trailing `poll()` in `socket_recv` moved
  `poll()` calls per round trip **3.30 → 2.31** exactly as intended — and total
  time in `poll()` went **45.8 → 44.8 µs/rt**, i.e. nowhere, because µs per
  call rose **13.9 → 19.4**. **The work is conserved; it just relocates.** So
  the per-call socket walk is *not* the dominant term at 66 sockets, and call
  count is not a lever. Measured +1.1 % at c=32, inside noise; reverted as
  unearned. This also weakens §11.1's table-size reading as an explanation *at
  this table size* — it was measured at 128-2048 slots under connection churn.
- ~~**`net-noalloc` async TX.**~~ Predicted ~40 % — see §5d. Measured
  **−14.6 %**. The spin it removes was never the cost.

What is actually left:

1. **The transport (§5d).** ~19 µs of the ~44 µs budget is one MMIO notify
   vmexit per packet, and it is the single largest remaining term. It is not
   addressable from inside the kernel — it needs a different netdev backend
   (`vmnet-*`, `tap`/`vhost`) or Firecracker. **Run the Linux-under-QEMU-SLIRP
   control first**: it decides whether any kernel-side gap remains at all.
2. **The smoltcp stack term** — ~21-22 µs/rt of real TCP work plus the socket
   walk. Not yet decomposed into "state machine" vs "walk"; the recv-poll
   experiment shows the split is not what §11.1 implied at this table size.
   Decompose before optimising.
3. **Delayed ACK** — done, §3, ~15 µs. The one confirmed win.
4. **The listener pools and the socket cap (§5b)** — 32 smoltcp sockets per
   listener against a 128 soft cap is what makes four idle `nc` listeners take
   the guest down. §11.2's reclaim-off-the-accept-path is still not done. This
   is a robustness fix, and on this evidence **not** a throughput one.

## Background

- [`BENCHMARK_PERFORMANCE_ATTEMPT_0.md`](BENCHMARK_PERFORMANCE_ATTEMPT_0.md) §4
  — "a fixed round-trip ceiling", the finding this document explains. Its §5
  argument that SLIRP is not the constraint ("a forwarder capped at 20,000
  operations per second could not do 247,525") compares ops to round trips;
  §1 here settles the question directly instead.
- [`AKUMA_NET_ISSUES.md`](AKUMA_NET_ISSUES.md) §3.2 (blocking transmit), §11.1
  (poll cost tracks table size), §11.4 (targeted wake, worse twice), §11.5
  (polls are ~10x laps), §12 (`tx_wait` named the largest cost left).
- [`REDIS_BENCHMARK_HOST_CONTENTION_LIVELOCK.md`](REDIS_BENCHMARK_HOST_CONTENTION_LIVELOCK.md)
  — the client-side livelock met while measuring this, root-caused separately.
- Harnesses: `scripts/benchmarks/redis_conc_sweep.py` (the concurrency sweep
  §11 item 4 asked for), `redis_smp_sweep.py` (boot-per-arm driver),
  `redis_bulk_check.sh` / `redis_bulk_ab.py` (delayed-ACK bulk guard).
