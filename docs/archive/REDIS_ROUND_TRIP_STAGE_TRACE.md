# The round trip, stage by stage — and the 688 µs that is not in the poll budget (2026-08-24)

**Status: read entirely out of the logs `1e92d22a` already checked in.** No new
boot, no new build. [`REDIS_ROUND_TRIP_CEILING.md`](REDIS_ROUND_TRIP_CEILING.md)
established that throughput at saturation is `1 / (µs in poll() per round trip)`
and left four items on a fix list. This document decomposes that budget stage by
stage, answers the lock-granularity question its §2 raises but never measures,
corrects two of its readings, and finds a term nine times larger than the whole
budget sitting outside it.

> **The one-line answer.** The `NETWORK` spinlock is **97.3 % utilized** and only
> **3.2 % contended** — it is saturated, not queued, so finer locks buy nothing.
> The SMP=1 → SMP=4 gain is **89 % cache residency**, not locking. And at `c=1`
> the round trip is 757 µs of which `poll()` is 69 — **688 µs of wake and
> schedule latency that `[NICSTAT]` cannot see.**

Sources: `logs/redis_why/boot_smp{1,2,4}.log`, `boot_ack1ms.log`,
`sweep_akuma-smp{1,2,4}.json`, `sweep_akuma_smp4.json`, `sweep_docker.json`,
plus `logs/nic_sock{128,2048}.log` for the socket-walk marginal cost.
Windows are the saturating ones only (`--min-rx 10000`), which is what
`scripts/benchmarks/nicstat_breakdown.py` selects.

---

## 1. The stage trace

Per round trip at SMP=4, from `boot_smp4.log` window 39 — 74,207 RX packets,
146,094 TX packets, 240,860 `poll()` calls in a 5,000 ms window. Every row is a
`[NICSTAT]` counter divided by RX packets, except the residual.

| Stage | Counter | µs/rt | Share |
|---|---|---:|---:|
| `NETWORK` acquire | `poll_wait` 165 ms | 2.22 | 3.2 % |
| RX replenish notify | `rx_post` 354 ms | 4.77 | 6.9 % |
| smoltcp stack + GC + prologue | residual | 31.23 | 45.1 % |
| `wake_all` over 128 slots | `wake` 103 ms | 1.39 | 2.0 % |
| TX notify × 1.97 pkt | `tx_wait` 2192 ms | 29.54 | 42.7 % |
| `rx_done` | `rx_done` 4 ms | 0.05 | 0.1 % |
| **Total inside `poll()`** | `poll` 5135 ms | **69.20** | 100 % |

Also per round trip: **3.25 `poll()` calls**, **1.97 TX packets**, **1.19 NIC
interrupts**, **83 live sockets walked**.

**The residual is a residual.** It is `poll_us` minus every instrumented
sub-span, so it absorbs `poll()`'s un-guarded prologue and epilogue as well as
the smoltcp state machine and the GC sweep. Treat it as an **upper bound** on
the stack cost proper.

**What is not instrumented at all:** every `with_network()` acquisition — the
recv/send syscalls make roughly 4-6 per round trip (`socket.rs:1413`, `:1420`,
`:1423`, `:1464`, `:1480`) and `with_network` (`smoltcp_net.rs:1359-1372`) has
no timing whatsoever. Also uninstrumented: the EL0 side, and the wake path that
§4 shows is the largest term in the system.

## 2. The lock: 97.3 % utilized, 3.2 % contended

§2 of the ceiling document argues from `poll()`'s exclusivity but never
publishes the wait. `nicstat::record_poll_wait` has been recording it since
`smoltcp_net.rs:1143`:

| arm | window | poll µs/rt | `poll_wait`/`poll` | wait µs/rt | share |
|---|---|---:|---:|---:|---:|
| SMP=1 | `boot_smp1` w=50 | 149.3 | 0 / 3094 ms | 0.00 | 0.0 % |
| SMP=2 | `boot_smp2` w=34 | 83.5 | 31 / 1712 ms | 1.51 | 1.8 % |
| SMP=4 | `boot_smp4` w=39 | 69.2 | 165 / 5135 ms | 2.22 | 3.2 % |
| SMP=4, ack 1 ms | `boot_ack1ms` w=28 | 39.1 | 217 / 3623 ms | 2.34 | 6.0 % |

Every window in each arm agrees to within 0.1 pp. **The absolute wait is flat at
~2.2 µs/rt and stops growing at two cores**; the share climbs only because the
denominator shrinks. That is a handoff, not a queue — consistent with §2's "only
two actors", the async-main drain loop and single-threaded `redis-server`.

### Utilization is the number that matters, and it is ~100 %

`poll_us` is timed from `smoltcp_net.rs:1130` — *before* the `PreemptGuard` and
the lock — to `record_poll` at `:1355`, *after* the wake pass. So it contains
both. Held time at SMP=4 w=39:

```
5135 ms (poll) − 165 ms (wait) − 103 ms (wake, outside the guard) = 4867 ms
                                                    of a 5000 ms window = 97.3 %
```

Holds cannot overlap, so that is a real utilization figure. **`NETWORK` is never
idle.** The arithmetic is also self-consistent (4867 ≤ 5000) and bounds the
uninstrumented `with_network` holds from above.

### Therefore: a per-socket netlock does not help

The question comes up because `crates/akuma-net/src/locks.rs` already contains
the design — `LOCK_LEVEL_NETWORK` (10) → `LOCK_LEVEL_SOCKET_TABLE` (20) →
`LOCK_LEVEL_SOCKET` (30), with ordering enforcement. **It is dead code.** Grep
for `acquire_network_lock` across `src/`, `crates/`, `userspace/` and only
`locks.rs` and `lock_tests.rs` answer. It also cannot work as written:

```rust
// locks.rs:226
let _guard = NETWORK_LOCK.lock();
NETWORK_LOCK_HOLDER.store(holder_id, Ordering::Relaxed);
mark_lock_held(LOCK_LEVEL_NETWORK);
// Note: The guard is dropped here, but the mark remains held
```

The guard drops at function return, so `NETWORK_LOCK` grants no exclusion. What
survives is `HELD_LOCKS`, a single global `AtomicU32` shared across all cores
rather than per-thread, so on `smp-shared` two cores corrupt each other's bits.
`LOCK_LEVEL_SOCKET` has no lock object behind it at all. It is Phase-1
scaffolding that was never wired up.

Three reasons finer granularity is the wrong lever, in increasing order of
finality:

- **smoltcp's API is single-owner.** `smoltcp_net.rs:1150` is
  `net.iface.poll(ts, &mut net.device, &mut net.sockets)` — one exclusive borrow
  of the interface, the device, *and* the whole `SocketSet` at once.
- **Most of `poll()` is not per-socket work.** Routing, neighbor cache, DHCP,
  retransmit timers, the `pending_removal` GC, the device rings. There is no
  socket to attribute it to.
- **The device is one device.** §5d of the ceiling document: ~15 µs of the
  budget is a single MMIO write to `QueueNotify` in which QEMU runs SLIRP's NAT
  and the host-socket write inline. One TX queue, one notify register.

And the part a per-socket lock *could* parallelize — `socket.send_slice(buf)` at
`socket.rs:1424`, a memcpy into a ring buffer — does not appear in §1's table
because it is not a term. Both callers then re-converge on
`smoltcp_net::poll()` two lines later (`socket.rs:1437`, `:1479`, `:1530`).

## 3. SMP=1 vs SMP=4: the residual collapses and nothing else moves

| Term (µs/rt) | SMP=1 w=50 | SMP=2 w=34 | SMP=4 w=39 |
|---|---:|---:|---:|
| poll total | 149.3 | 83.5 | 69.2 |
| ├ `tx_wait` | 36.4 | 28.8 | 29.5 |
| ├ `rx_post` | 4.6 | 4.0 | 4.8 |
| ├ `wake_all` | 5.5 | 2.1 | 1.4 |
| ├ lock wait | 0.0 | 1.5 | 2.2 |
| └ **residual** | **102.8** | **47.0** | **31.2** |
| `poll()` calls / rt | 3.32 | 3.43 | 3.25 |
| live sockets walked | 66 | 66 | 83 |
| **residual µs / poll call** | **31.0** | **13.7** | **9.6** |

The last row is the finding. **The same code, over *more* sockets (83 vs 66),
costs 3.2× more time per call on one core.** It is not doing more work — calls
per round trip are 3.32 vs 3.25 — and the socket count moves the wrong way for
any walk-cost explanation.

`tx_wait` per packet barely budges (18.8 → 14.7 → 15.0 µs) and `rx_post` is flat
(4.6 → 4.0 → 4.8). **Residual accounts for 71.6 µs of the 80.1 µs total gain —
89 %.**

The step pattern names the mechanism: 31.0 → 13.7 → 9.6. One core to two halves
it; two to four barely moves. That is the two-actor shape. On one core the
netpoll drain loop and `redis-server` timeshare, and every switch costs the poll
path its cache and TLB working set — a `SocketSet` of 66-83 sockets with their
RX/TX ring buffers, evicted by Redis's hash table on every quantum. On two cores
they stop switching; past two there is nothing left to separate.

**What this rules out.** The SMP gap is not lock contention — contention *grows*
from 0.0 to 2.2 µs as cores are added, in the opposite direction to the gap. It
is not the transmit path, which is flat. It is not the socket-table walk, which
is larger at SMP=4. It is memory residency, and no locking change addresses it.

## 4. The single-client hole: 688 µs nobody has measured

Everything above describes saturation. At `c=1` the picture inverts and nothing
in the existing analysis explains it.

| | c=1 rps | µs/rt | of which in `poll()` | unexplained |
|---|---:|---:|---:|---:|
| Akuma SMP=4, fresh | 1,321 | 757 | 69 | **688 (91 %)** |
| Docker / gvisor | 8,734 | 114 | — | — |

Three independent readings point at the same place:

- **Cores do not help.** SMP=2 gets 1,380 rps at c=1; SMP=4 gets **1,321** —
  slightly *worse*. A latency four idle cores cannot reduce is a wait, not a
  computation.
- **The c=2 → c=4 step is 4.8×.** 2,646 → 12,723 rps at SMP=4. Throughput that
  jumps that hard when a third and fourth client arrive is a fixed
  per-round-trip stall being overlapped, not a resource being filled.
- **The aged VM is 4.6× faster here.** `sweep_akuma_smp4.json` (label
  `akuma-smp4-fwd`, the long-running VM §5 of the ceiling document dismisses as
  "degradation") does **6,105 rps at c=1** against the fresh boot's 1,321, while
  plateauing at 7,634 against 14,085. Two arms that cross like that are two
  operating regimes, not degradation. The economical explanation is that
  something on the long-running VM keeps a core spinning, so a socket wake is
  picked up immediately instead of waiting for a scheduler tick — which helps
  c=1 exactly as much as it hurts c=32 by stealing a core from `poll()`.

Full sweeps, for the record:

| arm | c=1 | c=2 | c=4 | c=8 | c=16 | c=32 |
|---|---:|---:|---:|---:|---:|---:|
| akuma SMP=1 | 449 | 2,498 | 4,824 | 6,353 | 7,413 | 6,068 |
| akuma SMP=2 | 1,380 | 2,437 | 11,312 | 12,210 | 12,531 | 12,407 |
| akuma SMP=4 fresh | 1,321 | 2,646 | 12,723 | 13,441 | 13,831 | 14,085 |
| akuma SMP=4 **aged** | 6,105 | 7,123 | 7,508 | 7,541 | 7,740 | 7,634 |
| Docker | 8,734 | 15,699 | 22,422 | 36,101 | 51,546 | 64,516 |

**688 µs is nine times the entire saturated round-trip budget, and it is on no
fix list.** It is invisible to `[NICSTAT]`, which only instruments the inside of
`poll()`. §7 says where it most likely lives.

## 5. What Docker does better

Two different answers at two different client counts.

**At saturation: one vmexit per packet.** Docker Desktop's backend is gvisor
(confirmed from `--networkType gvisor` in its process arguments), a userspace
netstack in the same process as the port forwarder. Its entire round trip on
this host is 15.5 µs. Akuma pays 15.0 µs to hand *one* packet to QEMU — a cost
§5d shows is byte-independent (16-17 µs at 61 B and at 1288 B), does not
amortise across a 92,803-packet burst, and is unaffected by the transmit rings.
The single boundary crossing costs more than Docker's whole round trip.

**In the shape of the curve: per-connection parallelism.** Docker keeps scaling
from 8 to 32 clients (36k → 64k) because gvisor's netstack has per-endpoint
locking and a real SYN queue. Akuma is flat from four clients on: a listener is
a pool of `MAX_BACKLOG` pre-`listen()`ed sockets (`socket.rs:279`) rather than a
queue, and every socket operation funnels through one `NETWORK` spinlock that is
already 97.3 % utilized.

**The "~4× slower than Docker" figure is an artifact of where you stop
sweeping** — 1.8× at four clients, 4.6× at thirty-two — as the ceiling document
already says. At c=1 it is 6.6×, and §4 shows that number has nothing to do with
either mechanism above.

## 6. Two corrections to `REDIS_ROUND_TRIP_CEILING.md`

### 6a. §5b — the listener-pool walk is 5.5 %, not 50 %

§5b attributes the 50 % "smoltcp stack" share to `iface.poll()` walking the
whole `SocketSet`. `logs/nic_sock128.log` vs `logs/nic_sock2048.log` is exactly
that experiment and settles it:

| table | live sockets | µs / poll call |
|---|---:|---:|
| 128 | 123-127 | 10.3-11.4 |
| 2048 | 2043-2048 | 45.8-47.5 |

Marginal cost: **35.4 µs / 1920 sockets = 18.4 ns per socket visit** — and that
is an *upper* bound for the Redis case, because a 2048-socket table blows the
cache in a way a 66-socket one does not. Applied to `boot_smp4` w=39 (83 live
sockets, 3.25 polls/rt):

| | µs/rt | share of 69.2 |
|---|---:|---:|
| whole egress walk | 4.9 | 7.1 % |
| └ the 64 idle listener-pool entries | 3.8 | **5.5 %** |
| `wake_all` over all `MAX_SOCKETS` slots | 1.4 | 2.0 % |

§6 of the ceiling document already suspected this ("§6 later disproves that
reading… treat the split as undecomposed"), and §5b's own observation that 85
and 128 sockets both cost ~10.6 µs/call was the tell — a dominant walk would
have shown a 40 % gap there. The remaining ~31 µs is per-round-trip TCP state
machine, not table size.

**And smoltcp already skips the idle ones.** `tcp::Socket::dispatch`
(`smoltcp-0.12.0/src/socket/tcp.rs:2254`) opens with
`if self.tuple.is_none() { return Ok(()); }`, and `listen()` sets
`tuple = None` (`:885`). A pool entry that has not taken a SYN returns on branch
one. There is no per-socket work left to elide — only the iteration.

Nor can the entries simply be left out of the set: `socket_ingress` needs them
there to match an incoming SYN. The available levers are all trades — shrink
`MAX_BACKLOG` (but with no SYN queue the pool depth *is* the arrival ceiling),
or split into two `SocketSet`s polled at different rates (but `iface.poll` takes
one set, and connect latency grows). None of them is worth 5 % against §5d's
15 µs.

### 6b. §5d — the 6,486 µs worst case is host noise, present in both arms

§5d attributes a **6,486 µs worst-case** submit→completion to virtio notify
suppression under `net-noalloc`. A **6,591 µs `tx_wait` maximum appears in the
baseline blocking path**, which suppresses nothing:

| span | mean | worst observed | ratio |
|---|---:|---:|---:|
| `tx_wait`, SMP=4 | 15.0 µs | 6,591 µs | 439× |
| `tx_wait`, SMP=1 | 18.8 µs | 7,867 µs | 418× |
| `poll()`, SMP=4 | 21.3 µs | 6,729 µs | 316× |
| `poll()`, SMP=1 | 45.0 µs | 17,778 µs | 395× |

A single MMIO write charged 7.9 ms is not a slow write. The guest's clock is a
physical counter that keeps running while the vCPU thread is off the host CPU,
so a host-side deschedule mid-vmexit appears from inside as a multi-millisecond
instruction. **The worst case is ambient host noise common to both arms; the
90.9 µs *average* is the real signal in that comparison.** §5d's verdict on not
suppressing the notify still stands — it rests on the average.

## 7. Are the logs trustworthy? Yes at window scale

Window boundaries are sound. Across all four boot logs the `dt=` field is
5,000-5,002 ms in every window but one per log, which reads 5,020-5,026 ms — the
first window after boot. No counter wrap, no reset, no missing window.

| log | dt=5000 | 5001 | 5002 | outlier |
|---|---:|---:|---:|---:|
| `boot_smp1` | 40 | 15 | 1 | 5020 ×1 |
| `boot_smp2` | 27 | 6 | 2 | 5026 ×1 |
| `boot_smp4` | 34 | 11 | 1 | 5025 ×1 |
| `boot_ack1ms` | 29 | 4 | 1 | — |

**The per-window averages are safe.** §6b's outliers contaminate them only
slightly — one 6.6 ms spike is 0.30 % of SMP=4's 2,192 ms `tx_wait` total, so
even twenty per window would move it 6 %. They dominate every *maximum* on the
page, so **any p99 claim taken from these logs is measuring the host's
scheduler**, not the guest.

## 8. `PollResult::SocketStateChanged` — why it carries no handle

The obvious question is why `poll()` cannot say *which* socket changed, so
`wake_all`'s 128-slot walk could become a targeted wake. Three findings:

**Ingress structurally cannot tell you, and upstream has a TODO saying so.**
`socket_ingress` (`smoltcp-0.12.0/src/iface/interface/mod.rs:637`) returns
`PollIngressSingleResult::SocketStateChanged` **unconditionally** for every
consumed frame, with this comment directly above it:

```
// TODO: Propagate the PollIngressSingleResult from deeper.
// There's many received packets that we process but can't cause sockets
// to change state. For example IP fragments, multicast stuff, ICMP pings
// if they dont't match any raw socket...
```

The result is not accurate even as a boolean — ARP, ICMP and multicast all
report a socket state change. The handle is known four frames down in
`process_ethernet` → `process_ip` → `process_tcp`, inside `InterfaceInner`,
which has no access to the return value. Threading it up is the TODO.

**Egress could, cheaply.** `socket_egress` (`:657`) has `item.meta.handle` in
scope in the loop and sets the result inside the per-item `respond` closure. But
egress is not where a reply's wake comes from — an arriving request is ingress.

**smoltcp already ships the answer, and this crate already enables it.** Every
socket carries `rx_waker` / `tx_waker` (`tcp.rs:492-494`), woken from inside the
state machine at the exact transitions (`tcp.rs:848`, `:1327`, `:1960`, `:2072`,
`:2544`), and `crates/akuma-net/Cargo.toml:92` already builds smoltcp with
`"async"`. The socket wakes its own waker from the point where it knows who it
is — no return value has to carry anything. Akuma uses this in exactly four
places (`smoltcp_net.rs:1483`, `:1952`, `:1981`, `:2006`), all in the kernel's
async `TcpStream`; the syscall hot path uses the blunt walk instead.

Three things stand between them:

1. **`WakerRegistration` is one-shot and single-slot.** `wake()` does `.take()`;
   `register()` overwrites. `KernelSocket.wakers` is a `Vec<Waker>`
   (`socket.rs:200`) precisely because an epoll thread and a blocking `recv` can
   wait on one socket at once, and fork-shared fds multiply that. Registering B
   silently drops A.
2. **It moves the wake inside `NETWORK`.** `poll()` defers `wake_all` past
   `drop(guard)` on purpose (`smoltcp_net.rs:1305-1311`): taking `SOCKET_TABLE`
   under `NETWORK` is AB-BA against `socket_can_recv_tcp` et al., which hold
   `SOCKET_TABLE` → `NETWORK`. smoltcp's wakers fire from inside `iface.poll()`.
3. **The escape is to register `ThreadWaker`s directly.**
   `ThreadWaker::wake` (`crates/akuma-exec/src/threading/mod.rs:3569`) is
   lock-free — generation gate, sticky `WOKEN_STATES` store, a `WAITING→READY`
   CAS, then `trigger_sgi` / `wake_core`. It touches no socket table and no
   console, and the `WakeHandle` packs into the `RawWaker` data pointer with no
   allocation. Storing it *in* the smoltcp socket adds no lock edge.

**The prize is 1.4 µs/rt** — the entire `wake` term, 2.0 % of the budget, ~3 %
after delayed ACK. Correct, cheap, and small.

## 9. Feasibility of `REDIS_ROUND_TRIP_CEILING.md` §6's list

Checked against this host, 2026-08-24.

**§6.1 the transport — the Linux control is cheap and decisive; the backends are
gated.** `scripts/cargo_runner.sh:254` already builds the netdev line to reuse
(`user,id=net0,hostfwd=tcp::4444-:4444`); the control needs an aarch64 Linux
image with redis and `scripts/benchmarks/rtt_load.py` pointed at it. No kernel
work, and it closes or opens the whole list. For alternative backends,
`qemu-system-aarch64 -M virt -netdev help` (QEMU 11.1.0) offers `vmnet-host`,
`vmnet-shared`, `vmnet-bridged`, `tap`, `vhost-user`, `socket`, `dgram`. On
macOS `tap` needs a kext that no longer loads and `vhost-user` needs a separate
backend process; **`vmnet-*` is the only real option and needs `sudo`.** It is a
clean A/B — same kernel, one flag, read `tx_wait` µs/pkt — but it removes
`hostfwd`, so the load generator must target the guest IP. That is a harness
change, not a kernel change.

**§6.2 decompose the smoltcp term — half done above.** §6a puts the walk at
5.5 % and leaves ~31 µs of state machine. Finishing the split needs one
`nicstat` counter around `iface.poll()` alone, separating it from the GC sweep
and the prologue currently folded into the residual.

**§6.3 delayed ACK** — done, confirmed.

**§6.4 listener pools and the socket cap** — §6a's 5.5 % independently confirms
the list's own "on this evidence **not** a throughput one". Robustness only.

**Missing from the list: §4's 688 µs.** Larger than every item on it combined.

## 10. Extracting the readiness state machine — done 2026-08-24

**Landed as `crates/akuma-net-yarn`.** Unlike `akuma-scheduler` this is not a
host-only model: the kernel depends on it and runs the same code the tests
exercise, which is what makes the parity claim mean anything. `akuma-net`
supplies only the effects.

`wait_until` (`crates/akuma-net/src/socket.rs:684-777`) is where §4's 688 µs
lives — a single client's every round trip ends with `redis-server` parked in
`recv` and something having to wake it. It is already a state machine in
everything but structure:

- a wake **epoch** read before polling, because `wake_all()` drains and a wake
  landing during the poll loop would otherwise be lost (`:704`)
- a drain loop bounded at **64** (`:708`)
- `any_progress` diverging from `condition()` (`:715`, `:756`)
- **`fruitless_progress_rounds >= 4`** before relaxing anyway — added because
  aria2c's swarm traffic made `poll()` report progress on nearly every call, so
  the park branch never ran and the loop busy-spun holding the BKL until SMP=4
  hard-wedged, reproduced 2026-07-24 (`:769-774`)
- register → re-check → park ordering as the lost-wake correctness argument
  (`wait_park`, `:785`)

Every one of those came out of a live-VM incident, and `64` and `4` are unranked
magic numbers. The repo already has the pattern twice — `akuma-kacho` for
observe/decide/hysteresis, and `akuma-scheduler` as a host-only discrete-event
model deliberately kept out of `default-members` so a candidate can be ranked in
a second instead of a devbox boot.

### How old-vs-new is established

`tests.rs` carries `mod reference` — the pre-extraction branch structure at
`socket.rs:693-776` transcribed **verbatim**, kept in the original's shape
(mutable `fruitless_progress_rounds`, the `!any_progress` / `else` split, the
`>= 4` comparison) rather than refactored, so it can be diffed against the
source it came from. `machine_matches_the_shipped_loop_step_for_step` runs 400
seeded observation streams × 64 laps through both and asserts the same decision
*and the same park deadline* on every lap. No divergence. **Do not tidy the
oracle** — its value is being a transcription, not being clean.

One deliberate behavioural change: the timeout comparison is `saturating_sub`
where the shipped loop used a plain subtraction. §6b's host-deschedule spans
made "the clock appears to have run backwards" a real way to manufacture an
`ETIMEDOUT`.

### 10a. Direct thread wakers — `net-direct-waker`

`smoltcp_net::register_socket_waker` registers the waiter's `ThreadWaker` on the
smoltcp socket's own `rx_waker`/`tx_waker`, so the wake fires from inside
`process_tcp` at the state transition rather than from `wake_all`'s list walk
after `poll()` has released `NETWORK`. This is `net-waker-park`'s idea with §8's
loss window closed: both registrations are one-shot, but smoltcp's can only be
lost across a few instructions instead of a whole 64-poll drain.

Safe to fire with `NETWORK` held and IRQs masked **only** because
`ThreadWaker::wake` is lock-free — generation gate, sticky store, CAS, SGI, and
no `SOCKET_TABLE` or console. That is the entire safety argument; a waker whose
`wake` takes a lock would recreate the AB-BA `poll()` defers `wake_all` past
`drop(guard)` to avoid.

**Scope, stated so the A/B is not over-read:** it does *not* remove `wake_all`'s
128-slot walk — that is a separate change, and this currently adds a
registration on top of it. It tests the wake-**latency** hypothesis (§4's
688 µs at c=1), not the 1.4 µs walk cost.

### What this cannot answer

- **The calibration gate is not optional** for anything predictive built on top.
  `project_sched_sim_wake_latency` records `akuma-scheduler` giving confident
  wrong answers twice. The target here is concrete and already measured:
  reproduce **1,321 rps at c=1**, the **4.8× step from c=2 to c=4**, and the
  **flat 14,085 plateau** — all three, or a model ranks nothing.
- **Caches are invisible to it.** §3 finds cache residency is 89 % of the SMP
  gap. This machine can rank park-vs-spin policy and size `DEFAULT_DRAIN_BUDGET`
  and `DEFAULT_FRUITLESS_LIMIT`; it cannot say anything about why one core is
  3.2× slower per poll call. Keep the two questions apart or the wrong one gets
  answered confidently.
- **`ParkKind::LightSleep` is modelled, not implemented.** It encodes the root
  `Cargo.toml`'s hypothesis — targetable *and* woken by any NIC interrupt — and
  has no kernel arm because it needs a scheduler "light sleep" state.
  `active_wait_policy()` never selects it; `wait_park` degrades it to the
  shipping default rather than parking on a wake nothing can deliver.

## Background

- [`REDIS_ROUND_TRIP_CEILING.md`](REDIS_ROUND_TRIP_CEILING.md) — the document
  this decomposes. §2 (the ceiling equation), §3 (delayed ACK), §5 (the aged-VM
  gap, reopened here as §4), §5b (corrected in §6a), §5d (partially corrected in
  §6b), §6 (the fix list assessed in §9).
- [`AKUMA_NET_ISSUES.md`](AKUMA_NET_ISSUES.md) §8 (the waker-park work that
  produced `wait_until`'s epoch counter), §11.1 (poll cost tracks table size —
  measured at 128-2048 slots, which §6a shows does not extrapolate down),
  §11.2 (reclamation off the accept path, still not done).
- [`REDIS_BENCHMARK_HOST_CONTENTION_LIVELOCK.md`](REDIS_BENCHMARK_HOST_CONTENTION_LIVELOCK.md)
  — why `redis-benchmark` cannot measure this system.
- [`AKUMA_SCHEDULING_EXTRACTION.md`](AKUMA_SCHEDULING_EXTRACTION.md) — the
  host-only-model pattern §10 proposes reusing.
- Harnesses: `scripts/benchmarks/nicstat_breakdown.py` (produces §1 and §3
  directly), `redis_conc_sweep.py`, `redis_smp_sweep.py`, `rtt_load.py`.
