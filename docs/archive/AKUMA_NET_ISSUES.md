# Akuma networking: why a round trip costs milliseconds (2026-08-19)

**Status: measured, root-caused, FOUR fixes landed and A/B-verified — the
fourth (§9, the doorbell re-arm race) is a one-line reordering worth +65 %
throughput and 2.7x on p99, and brings p50 to parity with Linux. Two further
attempts — static RX/TX rings (§7) and waker-based parking (§8) — are written,
measured, and left OFF because both regress the tail; §8 also corrects an
attribution error in §7.4 that should be read before building on this
document.** This is the investigation
record for the NIC-path audit. The procedures it produced live in
[`../runbooks/debug-network.md`](../runbooks/debug-network.md); the numbers and
the reasoning live here.

> ### The one-line answer
>
> **There was no virtio-net interrupt.** `src/main.rs` registered exactly one
> device IRQ — 27, the timer. Every other part of the network stack was polled,
> and every loop that waited for a packet parked in `WFI`, which on this host can
> only be woken by a **3 ms** timer tick. A blocked socket reader was measured
> parking for **4.9 ms on average**. Everything else in this document is smaller
> than that.
>
> Registering that interrupt — plus a cross-core wake, because an SPI is
> delivered to one core while the waiter may be halted on another — took an
> HTTP request from **4,829 us to 657 us at p50**, against a shape-matched
> Linux control at **576 us**. A fourth fix (pressure-driven socket reclaim)
> removed a 26 % error rate. Details in §6.

---

## 1. What was added to measure it

Neither existing profiler could answer "where does a round trip spend its time".
`bkl-profile` attributes *lock* time by syscall tag — it says `netpoll` is the
top holder, which is true and not actionable. `PSTATS` is per-thread syscall
counts. Neither can separate device time from stack time from wait time.

| addition | what it measures |
|---|---|
| `crates/akuma-net/src/nicstat.rs` (feature `net-profile`) | per-packet RX/TX/poll timing and bytes at the virtio boundary, plus time parked in `blocking_relax` |
| `src/nic_profile.rs` | prints a windowed `[NICSTAT]` delta from the async-main loop |
| `scripts/benchmarks/bench_nic_rtt.py` | host-side round-trip latency (`connect` / `echo` / `http`), and parses the `[NICSTAT]` windows |
| `userspace/httpd/src/stats.rs` (`HTTPD_STATS=1`) | per-request phase split inside the server, so server cost can be subtracted |
| `scripts/benchmarks/serial_httpd_ref.py` | a shape-matched Linux control — single-threaded, one connection at a time, no cache, HTTP/1.0 close |

All of it is off by default and compiles to nothing when off (`net-profile`
mirrors `bkl-profile`'s discipline; every recorder is an empty inline body).

Build and run:

```bash
cargo build --release --features devbox-smoltcp,no-tests,net-profile
DISK=devbox.img MEMORY=4096 SMP=4 cargo run --release \
    --features devbox-smoltcp,no-tests,net-profile > logs/nic.log 2>&1 &
scripts/benchmarks/bench_nic_rtt.py --mode http --target localhost:8080 \
    -n 300 --nicstat logs/nic.log
```

---

## 2. The numbers

Machine: Apple silicon, `devbox-smoltcp`, `SMP=4 MEMORY=4096`. Docker arms
pinned to the same budget (`--cpuset-cpus=0-3 -m 4g`). Client is macOS in every
arm, reaching the server over that guest's host port forward.

### HTTP request — connect, GET a 23-byte file, read, close

| arm | server | req/s | p50 | p99 | errors |
|---|---|---:|---:|---:|---:|
| `docker-nginx` | nginx, `keepalive_timeout 0` | **1,716** | **556 us** | 842 us | 0 |
| `docker-serial` | `serial_httpd_ref.py` — httpd's exact shape, in Python | **1,641** | **576 us** | 882 us | 0 |
| `akuma-httpd` | `userspace/httpd` | **177** | **3,600-4,800 us** | 7,400 us | **26 %** |

The two Linux arms landing within 5 % of each other is the important control:
**server architecture does not matter at this scale.** An event-driven C server
and a single-threaded Python one that re-reads the file per request produce the
same number, so the ~9x gap to Akuma cannot be blamed on `httpd` being a serial
accept loop. (nginx alone would have been an unfair comparison, and was
challenged as such — hence the second arm.)

### Where httpd's own time goes (`HTTPD_STATS=1`)

```
httpd: [50 req] accept=324798us(99%) read=3us file=55us write=25us log=412us other=0us tx=23B/req
httpd: [50 req maxima] accept=16049427us read=6us file=54us write=65us log=530us other=2us
```

- **`other` — every byte of parsing, path building and formatting — is `0 us`.**
  Server compute is not measurable.
- `read` + `file` + `write` = **83 us**. That is the entire real work.
- **`log` = 412 us**, five times the real work: two `print!` lines per request
  onto a serial console where output is a per-byte MMIO store. Suppressed with
  `HTTPD_QUIET=1`.
- `accept` is everything else, and it is a *wait*.

So of a ~3.6 ms request, at most ~0.5 ms is the server. **The rest is kernel.**

### Device-level counters during the same run (`[NICSTAT]`)

```
[NICSTAT] w=11 dt=5002ms rx=249p/14kB tx=249p/13kB lo=0p/0kB drop=0
[NICSTAT] w=11 tx_wait=5ms(21.9us/pkt max=150us) rx_post=2ms(10.3us) rx_done=0ms
[NICSTAT] w=11 poll=3442c/244prog 10ms(3.1us/c max=263us) empty=3442 relax=1963/9997ms(5093.1us)
```

| observation | reading |
|---|---|
| `relax=1963/9997ms(5093.1us)` | 1,963 parks in `blocking_relax`, **5.1 ms each**. This is the whole latency budget |
| `tx_wait 21.9us/pkt, max 150us` | every transmitted frame blocks the caller ~22 us — with `NETWORK` held **and IRQs masked** |
| `rx_post 10.3us` | one MMIO notify per *packet*, because only one receive buffer is ever posted |
| `empty=3442` = `poll=3442c` | **100 %** of `Device::receive()` probes found nothing. The drain loop is pure overhead when idle |
| `poll` ~3,442 calls / 5 s | the async-main netpoll loop runs ~700 times a second, i.e. once per tick or two — not free-running |

---

## 3. Root causes, ranked by the evidence

### 3.1 No virtio-net RX interrupt — milliseconds

`src/main.rs:951-957` registers one device IRQ: 27 (the virtual timer). Nothing
registers the virtio-net SPI. Consequences:

- The async-main loop ends each lap with a bare `wfi` (`src/main.rs` ~1727).
  Its own comment says *"A pending RX/timer/SGI IRQ makes WFI return at once"* —
  **the RX half of that sentence is not true**, and the same claim appears in
  `threading::idle_halt` and in `socket::wait_until`'s comment about
  `blocking_relax` waking "on the next IRQ (RX under active traffic…)".
- A packet that arrives just after a poll is therefore invisible until the next
  3 ms tick. `pick_tick` chose 3,000 us on this host (`[Timer] host WFI probe:
  tick = 3000 us`); QEMU HVF declines to honour WFI below ~2.5 ms, so 1 ms is
  not available.
- Measured effect: `relax` averages 4.9-5.2 ms. A request needing two such waits
  lands at ~7 ms, which is exactly the observed p99.

**Blocker found while scoping the fix:** `src/gic_v3.rs:185-195` says *"SPIs
(>= 32) would use the distributor (unused by Akuma)"* and that affinity routing
via `GICD_IROUTER` **is not programmed**. Enabling the NIC's SPI is therefore
not a one-line `enable_irq` — the GICv3 driver needs real SPI support first
(`GICD_ISENABLER` plus `GICD_IROUTER` for the target core). That is the single
highest-value remaining piece of work in this document.

The handler itself must stay trivial: acknowledge the virtio interrupt with a
raw MMIO write to `InterruptACK` (0x64) using the base address captured at
probe, and return. It must **not** touch `NETWORK` — the interrupted core may
hold it. Waking `WFI` is the entire point; the netpoll loop does the rest. This
matches the discipline `no-bkl-irq` already relies on for the timer (raw MMIO
and atomics only), and `rust_irq_handler_with_sp` already dispatches any
non-SGI device IRQ BKL-free under that feature.

### 3.2 Transmit blocks the caller with IRQs masked — tens of microseconds

`VirtioTxToken::consume` calls `VirtIONetRaw::send`, which is
`VirtQueue::add_notify_wait_pop`: it notifies the device and then

```rust
while !self.can_pop() { spin_loop(); }
```

This runs inside `iface.poll()`, inside the `NETWORK` spinlock, inside a
`PreemptGuard` that masks local IRQs. So for 22 us per packet (150 us worst
case) **no core can enter the network stack and this core cannot take an
interrupt** — while it waits for the host's QEMU thread to be scheduled.

The fix is `transmit_begin` (non-blocking) plus reaping completions on a later
pass, which requires more than one TX buffer — see 3.3.

### 3.3 One RX buffer and one TX buffer

`VirtioSmoltcpDevice` holds `rx_buffer: [u8; 2048]` and `tx_buffer: [u8; 2048]`
and tracks a single `rx_token`. The virtqueue is 16 descriptors deep and the
device could hold a ring of buffers; instead exactly one is ever posted, so:

- every packet costs a fresh `receive_begin` → MMIO notify → vmexit (10.3 us
  measured);
- a burst cannot be drained without a full round of notify/complete per frame;
- TX cannot be made asynchronous at all, because the single buffer is still
  borrowed by the device after `transmit_begin` returns.

A static ring of buffers fixes all three at once, and is the honest reading of
"noalloc for TCP/IP": the packet path has no heap traffic to remove (the only
`vec!` is the loopback queue, and `lo=0p` in every NIC window), but it does have
a *missing* static buffer pool.

### 3.4 Socket slots exhaust under a round-trip workload — 26 % errors

At ~175 requests/s, a quarter of connections were reset. `MAX_SOCKETS` is 128 on
this build (`small-sockets` + `many-sessions`), and a closed socket sits in
`pending_removal` until TCP teardown completes or `SOCKET_GC_TIMEOUT_US`
(**30 s**) expires. Connection-per-request traffic retires far more sockets per
second than that GC allows, so the pool drains and `accept` starts failing.
`docs/runbooks/debug-network.md` already records the shape of this failure
("Panic: adding a socket to a full SocketSet"), but not that a modest HTTP rate
reaches it.

### 3.5 Console output is a real cost, and httpd was paying it per request

412 us/request of `print!`. Not a kernel bug, but it belongs in any latency
budget on this platform, and it is the reason
`archive/SERIAL_TRACE_TRAFFIC_AUDIT.md`'s rule ("histogram the log before
blaming the kernel") is in the runbook. Histogrammed here: 225 KB over ~15
minutes at idle, ~250 B/s — not a bottleneck at rest, but ~120 B per HTTP
request under load.

---

## 4. Results

Same harness, same guest, same Docker controls, four kernel builds. `httpd` is
identical across all four Akuma rows (the allocation-free, `HTTPD_QUIET=1`
build), so every difference below is kernel.

| | req/s | errors | min | p50 | p90 | p99 |
|---|---:|---:|---:|---:|---:|---:|
| Akuma baseline | 177 | 26 % | 3,035 us | 4,829 us | 6,336 us | 7,373 us |
| **+ virtio-net IRQ** (§3.1) | 363 | 26 % | 377 us | 2,148 us | 5,353 us | 6,602 us |
| **+ cross-core wake** (§6.2) | 942 | 26 % | 426 us | 645 us | 1,080 us | 2,202 us |
| **+ socket reclaim** (§3.4) | **1,036** | **0 %** | **394 us** | **657 us** | 1,880 us | 4,019 us |
| *Linux control (same shape)* | *1,641* | *0 %* | *519 us* | *576 us* | *643 us* | *882 us* |
| *Linux control (nginx)* | *1,716* | *0 %* | *498 us* | *556 us* | *613 us* | *842 us* |

**p50 went from 8.4x Linux to 1.14x Linux. Minimum latency is now better than
the Linux control** (394 us vs 519 us) — Akuma reaches the wire faster; what is
left is entirely in the tail.

`[NICSTAT]` from the final build confirms the mechanism rather than a
coincidence: `NIC interrupts 179` where the baseline had none, and `poll` rose
from 3,442 to 15,292 calls per 5 s window — the stack is now driven by arrivals
instead of by the tick.

httpd's own phase split on the final build, for the same 200 requests:

```
httpd: [200 req, 1071us/req wall] accept=700us(65%) read=132us(12%) file=114us(10%)
                                  write=117us(10%) log=3us(0%) other=0us(0%) tx=23B/req
```

- `other` is **still 0 us**. Server compute was never the constraint, before or
  after — which is the finding that makes the kernel attribution safe.
- `log` fell 412 us -> 3 us with `HTTPD_QUIET=1`: that 409 us was pure serial
  console.
- `accept` fell from ~99 % of the request to 65 %, and from milliseconds to
  700 us.

### What is left (the tail)

p90 is 1,880 us and p99 is 4,019 us against Linux's 643/882. `[NICSTAT]` still
shows `blocking_relax 1384 parks, 3103 us each` — i.e. some waits are still
landing on the 3 ms tick rather than on a packet. Two candidates, untested:

1. The doorbell coalescer (`NIC_WAKE_PENDING`, `src/main.rs`) is cleared only by
   the async-main drain loop. If a packet arrives while a broadcast is already
   pending but the drain has not run since, that packet gets no wake.
2. The remaining per-packet device costs of §3.2/§3.3 (25 us of IRQ-masked TX
   spin, 7 us per RX buffer post) are still present and unaddressed.

## 5. What was changed

| change | file | effect |
|---|---|---|
| **virtio-net RX interrupt** | `src/gic_v3.rs`, `src/gic.rs`, `src/main.rs`, `crates/akuma-net/src/smoltcp_net.rs` | §6.1 / §6.2 below. Default-on |
| **Pressure-driven socket reclaim** | `crates/akuma-net/src/smoltcp_net.rs` | §3.4. Default-on |
| Static RX/TX rings + async TX | `crates/akuma-net/src/virtio_rings.rs`, `smoltcp_net.rs` | §7. **OFF** (`net-noalloc`): halves the lock hold, loses 2.9x on p90 |
| Waker-based socket parking | `crates/akuma-net/src/socket.rs`, `runtime.rs`, `src/main.rs` | §8. **OFF** (`net-waker-park`): directed wake, but lossy — 1,071 → 944 req/s |
| **Doorbell re-arm before the drain** | `src/main.rs` | §9. **Default-on.** Closes a swallowed-wake race: +65 % req/s, p99 2.7x, p50 to Linux parity |
| NIC counters + `[NICSTAT]` dump | `crates/akuma-net/src/nicstat.rs`, `src/nic_profile.rs` | measurement only; off by default |
| Round-trip benchmark + Linux control | `scripts/benchmarks/bench_nic_rtt.py`, `serial_httpd_ref.py` | measurement only |
| httpd phase timing | `userspace/httpd/src/stats.rs` | measurement only, `HTTPD_STATS=1` |
| httpd allocation removal | `userspace/httpd/src/resp.rs` | compile-time error responses, stack-buffer 200 headers, per-second `Date` cache, reusable request/file buffers, streaming for large files |
| httpd quiet mode | `userspace/httpd/src/main.rs` | `HTTPD_QUIET=1` drops the 412 us/request of logging |

httpd p50 moved 7,415 us -> 3,624 us across the instrumented rebuild. **That
number must not be quoted as the value of removing allocation**: the same
rebuild also gated the per-request logging behind `verbose()`, and the error
rate differed between the two runs. The defensible statement is the phase split
in §2: compute was 0 us before and after, so allocation was never the
constraint — removing it was worth doing for its own sake, not for latency.

---

## 6. The interrupt fix, in detail

### 6.1 GICv3 had no working SPI path

`gic_v3::enable_irq`'s SPI arm wrote `GICD_ISENABLER` and nothing else, with the
comment "best effort; Akuma uses no SPIs". That was accurate: an SPI enabled
that way **never reaches the CPU**, because its reset state is Group 0 and the
kernel only enables Group 1 (`ICC_IGRPEN1_EL1`). The arm now programs, in the
order the architecture requires — configuration before enable:

1. `GICD_IGROUPR` — Group 1 Non-secure.
2. `GICD_IPRIORITYR` — 0xA0, the same value `init` gives SGIs/PPIs, below
   `ICC_PMR_EL1`'s 0xFF.
3. `GICD_IROUTER` — affinity 0.0.0.0, written explicitly rather than trusting a
   reset value the architecture leaves UNKNOWN.
4. `GICD_ISENABLER`, then wait for `GICD_CTLR.RWP`.

`GICD_ICFGR` is left alone: virtio-mmio is level-triggered and level is the SPI
reset state.

The INTID: QEMU wires virtio-mmio transport `i` to SPI `16 + i`, and an SPI's
INTID is `32 + spi`, so slot `i` is INTID `48 + i`. The slot is **probed**, not
assumed — `akuma_virtio::probe` reports where the NIC actually landed.

The handler (`nic_irq_handler`, `src/main.rs`) acknowledges the device with a
raw MMIO write and returns. It never touches `NETWORK`: the core it interrupted
may be holding it, and that is the AB-BA shape `PreemptGuard` exists to prevent.
Raw MMIO plus atomics is exactly what makes it legal on the `no-bkl-irq`
dispatch path, the same discipline the timer handler follows.

### 6.2 One interrupt is not enough — the cross-core doorbell

The interrupt alone moved the *minimum* from 3,035 us to 377 us but left the
median at 2,148 us. The reason is in `GICD_IROUTER`: an SPI is delivered to
**one** core, and the thread waiting for that packet is halted in
`blocking_relax` on whichever core it happened to be scheduled on. A halted core
only wakes on an interrupt *it* receives. So the fast cases were the ones that
happened to be waiting on the routed core.

`nic_irq_handler` now broadcasts the scheduler SGI (`gic::broadcast_sgi`, a
single `ICC_SGI1R_EL1` write with all 16 aff0 target bits) so every core's `wfi`
ends. That took the median from 2,148 us to 645 us.

Broadcasting per packet would cost `(cores - 1)` IPIs per frame, each entering
`sgi_scheduler_handler_with_sp` and contending `POOL` — the shape behind
`[SGI] POOL contended`. So the broadcast is behind a **doorbell coalescer**:
`NIC_WAKE_PENDING` is set by the handler and cleared by the async-main drain
loop, which bounds the SGI rate to how fast the stack polls rather than to how
fast packets arrive.

## 7. Static RX/TX rings and async transmit — done, measured, and OFF

`crates/akuma-net/src/virtio_rings.rs`, kernel feature `net-noalloc`
(→ `akuma-net/net-noalloc`). **Not in `default`.** It does what §3.2 and §3.3
said it would at the device layer, and it makes the end-to-end tail *worse*.
Both halves of that are the finding.

### 7.1 What it does

- **RX**: 8 slots of 2 KB in BSS, all posted up front, so the device always has
  somewhere to DMA. Replaces the single buffer that had to be re-posted per
  packet.
- **TX**: 8 slots. `transmit_begin` submits and returns; completions are reaped
  on a later pass (`TxRing::reap`, called before every claim). Replaces
  `VirtIONetRaw::send` — `add_notify_wait_pop`, which spins until the host
  consumes the descriptor, with `NETWORK` held and local IRQs masked.

Buffers are `static mut` rather than `NetworkState` fields because
`NetworkState` is built on the stack and then moved into the `NETWORK` static;
32 KB of inline arrays would go through a kernel stack during `init`. Same
discipline as the pre-existing `SOCKET_STORAGE`.

### 7.2 The device layer: it works

Per comparable 5,040-packet `[NICSTAT]` window, `devbox-smoltcp`,
`SMP=4 MEMORY=4096`:

| | single buffer | rings |
|---|---:|---:|
| tx blocking wait | 27.8 us/pkt, **140 ms**/window | 9.2 us/pkt, **46 ms**/window |
| total poll time | 472 ms | **211 ms** |
| per poll call | 10.7 us | 5.7 us |
| NIC interrupts | 6,071 | 4,879 |
| `blocking_relax` parks | 3,918 | **4,644** |
| `tx_stall` / `orphan` | — | **0 / 0** |

94 ms of IRQ-masked spinning and 261 ms of `NETWORK`-lock-held time removed per
5 s window — over half the lock hold. The ring never saturated (`tx_stall=0`)
and never desynchronised from the used ring (`orphan=0`).

### 7.3 End to end: it regresses

Same `httpd` binary in both arms (`md5 6085451088b1545f5256892312c698f0`), same
docroot, 400 requests per run, 10 runs for rings (across two boots) and 5 for
the control. Medians:

| | single buffer | rings | |
|---|---:|---:|---|
| req/s | 1,071 | 855 | **-20 %** |
| min | 378 us | 394 us | flat |
| p50 | 658 us | 639 us | -3 % |
| p90 | 1,172 us | **3,433 us** | **2.9x worse** |
| p99 | 4,091 us | **6,819 us** | **1.7x worse** |

Forcing the transmit notify (§7.5) recovers part of this — 938 req/s, p90
2,930 us, p99 5,384 us — and is kept, but does not close the gap.

### 7.4 Where it went: `accept` — but see the correction below

`HTTPD_STATS=1`, in-load blocks only (the ~11 ms blocks in the log are the idle
gaps between benchmark runs landing in `accept`, not measurements):

| phase | single buffer | rings |
|---|---:|---:|
| `read` | 44-78 us | **7-9 us** |
| `file` | 98-136 us | 49-122 us |
| `write` | 62-114 us | 50-111 us |
| `other` (compute) | 0-1 us | 0-1 us |
| **`accept`** | 546-741 us (66-77 %) | **882-1,262 us (81-88 %)** |
| wall/req | ~925 us | ~1,259 us |

Every syscall got cheaper — `read` by 6x, precisely the reduced lock hold from
7.2 showing up where predicted. `other` is still 0 us, so server compute remains
irrelevant, as in §2.

> **Correction (2026-08-20).** The `accept` row was originally read as "the
> kernel wake is starved", and §8 was built on that. **It does not support that
> conclusion.** `bench_nic_rtt.py` is serial — one connection at a time — so
> httpd's `accept` phase spans from finishing request N to connection N+1
> *arriving*, which includes the client's entire turnaround. Note that `accept`
> (546-741 us) sits on top of the client-measured p50 RTT (658 us), because it
> largely IS that RTT. `accept` growing means the whole round trip grew; it is
> not independent evidence about where inside the round trip the time went.
>
> A harness with concurrent connections in flight would separate the two. Until
> then, treat `accept` as a restatement of end-to-end latency, not a decomposition
> of it.

### 7.5 The notify hypothesis — tested, real, and not enough

**Hypothesis.** `transmit_begin` notifies the device only when
`VirtQueue::should_notify()` allows, and QEMU negotiates `VIRTIO_F_EVENT_IDX`,
so that can return false. `VirtIONetRaw::send` checks the same flag — but then
spins until the used ring advances, which waits the suppression out and forces
the host to consume the frame. Async submit has no such backstop.

**Test.** Two changes, then the same 5-run sweep:

1. A new counter, `[NICSTAT] tx_flight` — µs from `transmit_begin` to the
   completion being observed in the used ring. Once transmit stops blocking,
   `tx_wait` only covers the submit; `tx_flight` is where the rest of the cost
   went.
2. `smoltcp_net::nic_kick_tx()` — an unconditional write of the transmit queue
   index to virtio-mmio `QueueNotify` (0x050) after every submit, using the
   `NIC_MMIO_BASE` the IRQ handler already records. A spurious notify is a hint
   by spec, so it does not need to second-guess `should_notify`.

Also added: `reap` now runs once per poll lap (top of `Device::receive`), not
only inside `claim`. Without that, a slot stays in flight for as long as nothing
transmits — on request/response traffic, the entire gap between requests.

**Result.** The suppression was real, and fixing it is a genuine but partial
win:

| | rings | rings + forced notify | single buffer |
|---|---:|---:|---:|
| `tx_flight` avg | 90.9 us | 76.7-83.1 us | n/a |
| `tx_flight` max | **6,486 us** | **3,673-3,896 us** | n/a |
| `tx_wait` (submit) | 9.1 us/pkt | **17.4 us/pkt** | 27.8 us/pkt |
| req/s | 855 | **938** | **1,071** |
| p50 | 639 us | 653 us | 658 us |
| p90 | 3,433 us | **2,930 us** | **1,172 us** |
| p99 | 6,819 us | **5,384 us** | **4,091 us** |

The `tx_flight` worst case nearly halved and p99 fell 21 %, so notify
suppression was **a** cause. It is **not the dominant one**: p90 is still 2.5x
the single-buffer path. The kick also costs ~8 µs/packet of vmexit, which is why
`tx_wait` doubled — still well under the 27.8 µs it replaced.

**Caveat on `tx_flight`, stated because it is easy to over-read.** Reap runs
once per poll lap and there are ~38,000 laps per 5 s window — one per ~130 µs.
Sampling granularity alone can account for most of the ~80-90 µs mean, so
**`tx_flight` is an upper bound on device latency, not a measurement of it**.
The remaining max of ~3,700 µs is also suspiciously close to the 3 ms scheduler
tick, which suggests what it is really catching there is a poll lap that did not
happen (the core parked in `WFI`) rather than a device that was slow. A TX
completion wakes nobody — only RX raises the interrupt — so after the last
submit of a burst, nothing observes the completion until the next tick. That
makes the counter useful for the *avail*-side question it was built for and
unreliable as an absolute.

**Still unexplained**, and where the next attempt should go: the single-buffer
path takes ~25 % *more* NIC interrupts for the same packet count (6,071 vs
4,741-4,909 per 5,040 frames), because it must re-post a receive buffer — and
therefore notify — per packet. Those notifications may be doing double duty as
the wake source. The rings amortise exactly them away. That would mean the win
and the loss have the same cause, and the fix is an explicit wake rather than a
shallower ring.

### 7.6 A bug the measurement did not catch, found by reading

The first version fell back to `VirtIONetRaw::send` when every TX slot was in
flight. `send` is `add_notify_wait_pop`, and `pop_used` rejects any token that
is not the used-ring **head** (`Error::WrongToken`) — which is guaranteed to be
the case in the only situation that fallback is reached. It would have failed
the send *and* leaked the descriptor chain permanently, shrinking the send queue
until the NIC died. **`send` and `transmit_begin` cannot be interleaved on one
queue; the ring must own it exclusively.** Replaced with a bounded reap-spin
(`CLAIM_SPINS`) that drops the frame if the ring stays full, counted by
`TX_STALLS` and printed as `tx_stall` in `[NICSTAT]`.

### 7.7 Allocations in the packet path, after the change

Audited because "noalloc" was the premise:

- `virtio_rings.rs` and `nicstat.rs`: **zero** allocations. The rings hold only
  tokens, indices and lengths; the frames are BSS.
- The per-packet **external** path (RX complete → smoltcp → TX submit)
  allocates **nothing**, and allocated nothing before either. There was never
  anything to remove — the win available here was the spin, not the heap.
- **One per-packet allocation remains**: `smoltcp_net.rs`, the loopback TX arm —
  `vec![0u8; frame.len()]` plus a `VecDeque::push_back` per frame. It never
  fires for external traffic (`lo=0p` in every window measured here) but fires
  **once per frame for in-guest 127.0.0.1 traffic**. This is the same path that
  raises no interrupt, so any traffic-adaptive netpoll backoff must count
  loopback depth as well.
- Socket rx/tx buffers, the DNS socket and UDP packet metadata allocate at
  socket creation, not per packet.

## 8. The waker-park experiment — tested, WORSE, kept off

Kernel feature `net-waker-park` (→ `akuma-net/net-waker-park`). **Not in
`default`.** Second negative result of the session, and the more instructive one.

### 8.1 The premise, and why it looked airtight

`wait_until` (`crates/akuma-net/src/socket.rs`) is the blocking path for
`accept`/`recv`/`send`/`connect`. It parked in `blocking_relax()` — `yield_now`
+ WFI — which leaves the thread **READY**, never WAITING. A READY thread cannot
be targeted: `ThreadWaker::wake` CASes WAITING→READY and IPIs the thread's last
core, and both are no-ops against a thread that never parked properly.

So `smoltcp_net::poll()`'s `wake_all()` — which fires on every
`SocketStateChanged` — walked an **empty waker list** for every blocking socket
op. The only registrant of `socket_add_waker` was `poll.rs:478`, the epoll path.

Every other blocking path in the kernel already parks properly:

| subsystem | blocks with | woken by |
|---|---|---|
| pipe | `pipe_check_set_reader` registers a `WakeHandle` → `schedule_blocking` | `wake_by_handle` on write |
| fs / stdin, msgqueue | `schedule_blocking(deadline)` | targeted wake |
| epoll / ppoll | `socket_add_waker` → `schedule_blocking(deadline)` | `wake_all()` |
| **blocking socket ops** | **poll x64 → `blocking_relax()`** | **nothing — it re-polls** |

Sockets were the only outlier. Fixing an outlier that is also the slow path is
about as good as a hypothesis gets.

### 8.2 What was built

`wait_until` gained the socket index and the pipe discipline — **register, then
re-check, then park**:

```
socket_add_waker(idx, current_waker());   // announce BEFORE the last check
if condition() { return }                 // catch a wake that already landed
park_until(min(caller_timeout, now + 3ms))
```

Two `NetRuntime` seam entries (`park_until`, `current_waker`) because akuma-net
does **not** depend on akuma-exec — the `runtime.rs` comment claiming it does is
stale, left over from when `PreemptGuard` lived there. The 3 ms backstop is the
scheduler tick deliberately, not `poll.rs`'s 10 ms `BLOCKING_POLL_INTERVAL_US`,
so a missed wake is never worse than the behaviour it replaced.

### 8.3 Result: worse

Five runs x 400 requests per arm, same httpd binary, medians:

| | default (`blocking_relax`) | `net-waker-park` |
|---|---:|---:|
| req/s | **1,071** | 944 |
| p50 | **658 us** | 706 us |
| p90 | **1,172 us** | 2,169 us |
| p99 | **4,091 us** | 5,507 us |

### 8.4 Why — an imprecise-but-plentiful wake beat a precise-but-lossy one

The premise was true and irrelevant. `blocking_relax`'s WFI ends on **any**
interrupt, and under load the NIC raises ~6,300 per 5 s window — one every
~0.8 ms. The old wake was not missing; it was promiscuous and frequent.

The replacement is precise and **lossy**: `wake_all()` *drains* the waker list,
so a waiter must re-register on every lap, and any wake arriving during its
poll x64 window finds nothing registered and drops through to the 3 ms backstop.

The counters show the trade exactly:

| | default | `net-waker-park` |
|---|---:|---:|
| `relax` parks | 3,918 | **2,565** |
| us per park | 1,172 | **1,787** |
| total parked / 5 s window | 4,592 ms | 4,583 ms |
| `poll` per call | 10.7 us | 13.7-15.1 us |
| httpd `accept` | 546-741 us | 626-918 us |

Registration *works* — a third of the parks were replaced by directed wakes.
But the parks that remain are longer, because they are the ones that lost the
race and hit the backstop. Total parked time is unchanged, and the per-lap
registration cost is new.

**To beat the default this needs a park that is both targetable and woken by any
NIC interrupt** — a scheduler change (a "light sleep" state), not a network one.
A non-draining waker list would also narrow the race, at the cost of stale
registrations. Neither is attempted here.

### 8.5 A second, independent reason to leave it off: the BKL window

`sys_accept` opens a `NetBklGuard` for the whole syscall
(`src/syscall/net.rs:249`), so `socket_accept` → `wait_until` runs **inside a
dropped-BKL window** (`no-bkl-network`). With this feature on, `wait_until`
calls `schedule_blocking()` there — a real deschedule inside a BKL-free window.
`docs/reference/subsystems/locking.md` warns about precisely that:

> Any future BKL-free window that can span a `schedule_blocking()`/context-switch
> point (not just a single bounded I/O op) should be treated as touching [the
> process-table row], whether or not it looks like it does.

Phase 7b tried that shape on `ppoll`/`epoll_pwait` and got one intermittent
data-corruption run out of two, then reverted
([`BKL_PHASE7B_PPOLL_CARVE_OUT.md`](BKL_PHASE7B_PPOLL_CARVE_OUT.md) §3-4).

**Stated fairly: this deepens an existing shape rather than introducing a new
one.** The default path already spans a context switch, because `blocking_relax`
begins with `yield_now()`. But it deepens it from "yields and comes straight
back" to "descheduled for up to the 3 ms backstop", which widens the window the
process-table row is about. Nothing was observed — the boot suite is green on
this feature — but it was never run long enough to trip an intermittent
corruption, and it should not be turned on without that regimen.

### 8.6 What survives regardless

- `KernelSocket::waker_count()` — a diagnostic that reads zero for any blocking
  socket waiter on the default build. That is the outlier of 8.1, still true.
- `test_socket_wait_backstop_no_hang` — a waiter nothing ever wakes must return
  on its timeout, not hang. Guards the property that made the change safe to try.
- `test_socket_wait_registers_waker` (feature-gated) — proves registration
  happens while parked. It fails on the old code, and a latency assertion would
  not have caught it, because the backstop returns at about the same time either
  way.

## 9. The doorbell re-arm race — FIXED, and the biggest win of the session

`src/main.rs`. **Default-on**, no feature gate: it is a one-line reordering with
no tradeoff to opt into.

### 9.1 The race

The netpoll drain re-armed the NIC doorbell *after* draining:

```rust
while poll() { ... }                     // drain
NIC_WAKE_PENDING.store(false, Relaxed);  // re-arm — AFTER
```

while the interrupt handler broadcasts only on a clear doorbell:

```rust
if !NIC_WAKE_PENDING.swap(true, AcqRel) { gic::broadcast_sgi(SGI_SCHEDULER); }
```

A packet arriving **after the last `poll()` returned false but before the
`store(false)`** is missed by the drain, finds the doorbell already set so its
handler raises **no broadcast**, and is then erased by the re-arm. Every core
reaches the trailing `wfi` and sleeps to the tick. One swallowed wake, one 3 ms
request.

### 9.2 The fix

Re-arm **before** the drain. That inverts the race into a harmless one: a packet
landing during the drain now finds the doorbell clear, broadcasts, and leaves an
SGI pending that makes the trailing `wfi` return immediately, so the next lap
drains it. The cost is one extra SGI per drain that overlaps an arrival — still
bounded by the coalescer, since a burst mid-drain rings once, not once per frame.

No lock or IRQ guard is needed, and neither would help: a local `IrqGuard`
cannot exclude a *peer* core's handler, it would delay the very interrupt that
ends the `wfi`, and it would extend an IRQ-masked region across a drain that
costs 800+ ms per 5 s window under load — the exact cost §3.2 exists to complain
about. The ordering makes the race benign rather than excluded, which is
strictly better.

### 9.3 Result

n=2000 per run (n=400 is too few to resolve p90 here — the baseline's p90 ranged
1,143-5,048 us across runs at that size). A-B-A: doorbell measured, reverted to
baseline, then restored and re-measured, on separate boots. 6 doorbell runs,
5 baseline runs. Medians:

| | baseline | **doorbell** | Linux control |
|---|---:|---:|---:|
| req/s | 673 | **1,108** (+65 %) | 1,641 |
| p50 | 630 us | **583 us** | 576 us |
| p90 | 4,892 us | **2,107 us** (2.3x) | 643 us |
| p99 | 10,913 us | **4,085 us** (2.7x) | 882 us |
| p90 spread across runs | 1,143-5,048 us | **1,977-2,233 us** | — |

**p50 is now at parity with Linux** (583 vs 576 us). The collapse in *variance*
is the strongest evidence that this was the race it looked like: the baseline's
p90 swings 4.4x run to run, the fixed build holds within 6 %.

### 9.4 The mechanism, confirmed in the counters

| per 5 s window | baseline | doorbell |
|---|---:|---:|
| `relax` parks | 3,918 | **5,328-6,151** |
| us per park | 1,172 | **759-877** |
| `nic_irq` | 6,071 | 6,468 |
| total `poll` time | 472 ms | **362 ms** |

More parks, each much shorter, and slightly more interrupts — exactly what
closing a swallowed wake predicts. Waiters that used to sleep to the tick are
now woken early, so they park more often and for less time.

### 9.5 What is left in the tail, quantified

The host tick is **3,000 us exactly** (`[Timer] host WFI probe: tick = 3000 us`).

```
p50 (a normal request)            583 us
one tick                        3,000 us
tick + p50                      3,583 us
p99 measured                    4,085 us   -> 502 us unaccounted
p90 measured                    2,107 us   -> p90 - p50 = 1,524 us, LESS than a tick
```

So a p99 request is "one swallowed-wake-equivalent plus a normal request", with
~500 us left over — and p90 is *not* a clean one-tick event, so the tail is a
continuum rather than a second clean bimodal step.

**The next lead is in the same dump: `poll max = 3,592-3,695 us`.** A single
`poll()` call blocking for a full tick means a thread is holding `NETWORK` and
going to sleep while holding it — the same class of defect as this one (a wait
that lands on the tick), one layer down. `smoltcp_net::network_holder_snapshot`
already exists to attribute it. That is where to go next.

## 10. Herd polling: 5-14 % of contended BKL, and rate-limiting it does NOT help

Third negative result of the session, and the one that most changes how to read
the profiler.

### 10.1 What the attribution said

`bkl-profile` under HTTP load, `SMP=4`, three consecutive windows:

| tag | w2 | w3 | w4 (heaviest) |
|---|---:|---:|---:|
| irq/sched | 30.4 % | 32.7 % | 48.5 % |
| idle | 11.9 % | 19.9 % | 36.9 % |
| **netpoll_herd** | — | **14.3 %** | **5.0 %** |
| **netpoll_maint** | **15.2 %** | **10.6 %** | — |
| netpoll_drain | — | — | 2.2 % |

`netpoll_herd` is a `try_read()` on a channel that is usually empty and often
absent entirely, executed once per async-main lap — **15,412 laps/s** measured
under load, against the ~100/s `BKL_PHASE7_AUDIT.md` §2.6 assumed when it decided
not to carve `netpoll_maint` (that audit predates the NIC interrupt, so its
premise is 150x stale). Meanwhile `netpoll_drain`, the part that IS carved out,
is 2.2 %. It looks like free money.

### 10.2 It is not

Rate-limited herd polling to 100 ms (a ~1,500x reduction), A/B on the same
machine state, 4 runs x 2000 requests per arm:

| | herd every lap | herd every 100 ms |
|---|---:|---:|
| req/s | **1,040** | 1,002 |
| p50 | 605 us | 608 us |
| p90 | **2,453 us** | 2,727 us |
| p99 | **4,162 us** | 4,334 us |

Neutral on p50, slightly worse everywhere else, consistently across runs.
Reverted.

### 10.3 Why — the lesson worth keeping

**BKL contention share is not a proxy for throughput.** The `spins` a tag
accumulates measure how long *other cores waited* while that tag held the lock.
In this workload those other cores are overwhelmingly in `idle` (36.9 %) and
`irq/sched` (48.5 %) — together 85 % — i.e. they had nothing better to do. Giving
an idle core its lock back sooner buys nothing.

A secondary effect probably explains the small *regression*: removing work from
the tail of the lap makes the loop reach `wfi` sooner, so it runs more laps, so
it does more BKL acquire/release churn per second. Same shape as §8's finding
that a plentiful-but-imprecise wake beat a precise one — doing less per lap is
not the same as doing less work.

**Practical rule: use `bkl-profile` to find *who holds the lock*, not to predict
what a fix is worth. Only the end-to-end A/B decides that** — and only against a
control measured on the same machine state. (Today's control was 1,040 req/s
where the same build measured 1,108 a few hours earlier: ~6 % drift, enough to
manufacture or hide a result this size.)

### 10.4 What was kept

`netpoll_drain_step()` — the drain extracted from the async-main loop body into
its own `#[inline]` function. Behaviour-identical and measured as such; it exists
so a dedicated network thread can call it at packet rate while housekeeping runs
at its own cadence, with single-core/`extreme-size` builds keeping one thread
that calls both. That split is designed but NOT built or measured; §10.3 is the
reason to measure it end-to-end before believing the attribution's promise.

## 11. Not done

1. **The split netpoll thread** (§10.4) — `netpoll_drain_step()` is extracted and
   ready; the design is a dedicated drain thread at packet rate plus housekeeping
   at its own cadence, with `extreme-size`/`SMP=1` keeping one thread that calls
   both inlined halves. §10 is the warning attached: the attribution says
   maintenance+herd is 15-25 % of contended BKL, and §10.2 shows that number does
   not convert into throughput. Measure end-to-end against a same-session control.
2. **The `poll()` full-tick stall** — `poll max` is 3.6-3.7 ms. NOTE: the earlier
   guess that a `NETWORK` holder sleeps while holding it is **wrong** —
   `smoltcp_net::poll()` wraps the whole critical section in `PreemptGuard`, which
   masks IRQs, so the holder cannot be descheduled. Two live candidates instead:
   `NETWORK` is a plain (unfair) `Spinlock`, so a waiter can be starved under
   4-core contention; and `poll_us` spans the post-drop `wake_all()` pass, which
   walks **every** socket slot (`MAX_SOCKETS` = 128, or 256 with `many-sessions`)
   taking each one's waker lock. Attribute before fixing.
3. **Turn `net-noalloc` on once the tail is fixed** (§7) — the rings halve the
   lock hold and lose on p90 today. Their suspected loss mechanism (the
   per-packet `receive_begin` notify doubling as a wake source) is a wake
   problem, and §9 fixed a different wake problem, so **the rings deserve a
   re-measure on top of §9 before any further judgement**. Also still unmeasured
   against a pipelined workload (redis), the shape they should suit.
4. **A concurrent-connection harness.** `bench_nic_rtt.py` is serial, which is
   what confounded the `accept` reading (§7.4 correction). Without connections
   in flight, server-side phase timing cannot be separated from client
   turnaround.
5. **`userspace/httpd` does not run on Linux** — `hello` (the minimal libakuma
   binary) SIGSEGVs too, so the blocker is in libakuma's startup, not httpd.
   Worth fixing: it would give the same-binary A/B that
   `scripts/probes/` provides for `std` binaries. Syscall numbers are already
   Linux aarch64 apart from the 300+ Akuma extensions (`SPAWN` 301, `TIME` 305,
   `UPTIME` 319), and those return `-ENOSYS` rather than faulting.
6. **`connect`-mode comparison against Docker is not meaningful** — Docker
   Desktop's proxy opens a fresh backend connection per inbound connection
   (measured p50 137 us) where QEMU SLIRP does not (Akuma p50 120 us, i.e.
   *faster*). Use `connect` for Akuma-vs-Akuma A/B only.

## 11. Socket table, wake targeting, and four corrections (2026-08-20)

### 11.1 The socket table saturates to whatever cap it is given

Three arms, same session, 4 runs x 2000 requests, medians:

| arm | req/s | p50 | p90 | p99 | table settled at |
|---|---:|---:|---:|---:|---|
| **A** 2048 hardcoded | 848 | 808 us | 2,471 us | 5,348 us | **2047/2048** |
| **B** 128 + reclamation | 1,052 | 600 us | 2,353 us | 4,150 us | 123-127 |
| **C** soft 128 -> x1.2 -> 512 | **1,098** | **581 us** | **2,093 us** | **4,119 us** | **128, never grew** |

**Bigger is worse, not neutral.** `iface.poll()` walks the whole `SocketSet` every
call, so per-poll cost tracks the table: **10.6 us at 128 slots, 45 us at 2048**
(poll time 845-1,044 ms vs 1,807-2,862 ms per 5 s window). And the set fills to
whatever cap exists, because smoltcp holds `TimeWait` for `CLOSE_DELAY` = **10 s**
and at ~1,000 conn/s that wants 10,000 slots. Saturation is arithmetic, not tuning.

C is the design that landed. It is **statistically indistinguishable from B** here
(the ranges overlap and the cap never moved), because `grow_soft_cap` is gated on
`freed == 0` and reclamation always freed something. That gate is the point:
growing on `TimeWait` pressure would walk the cap to the ceiling and inherit A's
result. C earns its place on robustness — headroom for genuinely-live pressure
that B refuses outright — not on throughput.

`SOCKET_SOFT_CAP_START` is derived as `min(128, MAX_SOCKETS)` so `extreme-size`
starts at its own 32-slot ceiling and can never grow.

### 11.2 What the cliff actually is

At exactly `MAX_SOCKETS`, every `socket_create` — i.e. every accept, since
`socket_accept` refills the listener backlog — runs `reclaim_pending_slots`, a
nested scan (for each of 8 slots to free, rescan all of `pending_removal`) inside
`with_network`, so under `PreemptGuard` with IRQs masked. Observed:

| sockets | req/s | us/poll | relax parks | us/park |
|---:|---:|---:|---:|---:|
| 127 | 1,102 | 10.2 | 11,608 | 364 |
| **128** | **459** | **19.4** | 4,769 | **889** |

A threshold, not a gradient. Moving reclamation off the accept path — proactively
from the netpoll drain at a high-water mark — is still the fix, and is NOT done.

### 11.3 Wake accumulation (epoch): correct, and almost never fires

`wait_until` now reads `smoltcp_net::poll_count()` before polling and re-checks it
before parking, looping instead of parking if the stack moved underneath. This is
the lossless version of what §8's waker registration failed to be — `wake_all()`
drains its list, a counter cannot be lost.

It fires on **1.1-1.3 % of would-be parks** (155/11,608, 63/4,769, 65/5,704), and
end-to-end it measures neutral (1,028 vs 1,038 req/s, p99 4,166 vs 4,168). Kept:
it is one relaxed load, and the `epoch_saves` counter is what turns "no effect"
into a conclusion rather than a shrug.

### 11.4 Targeted wake instead of broadcast: tried twice, WORSE both times

The NIC handler broadcasts `SGI_SCHEDULER` to every core. Measured cost: **2.5
async-main laps per NIC interrupt**, waking cores with nothing to do. Two attempts
to wake only the cores with a parked socket waiter (`trigger_sgi_core` against a
per-core record maintained around `blocking_relax`):

| | laps per packet | req/s |
|---|---:|---:|
| broadcast | **3.18** | **~1,100** |
| bitmask of waiter cores | 1.5-1.8 | 569-867 |
| per-core waiter counts | 6.5-7.6 | 454-867 |

The bitmask lost wakes outright — two waiters on one core share a bit, so the
first to wake clears it and the second is never woken again (classic signature:
fewer `relax` parks, each 2-3x longer). Counting per core fixed that and was still
worse.

**Why targeting cannot work as written**, and both reasons are structural:

1. `blocking_relax` begins with `yield_now`, so a waiter can resume on a core it
   never marked. Any affinity it records is stale by the time the interrupt reads it.
2. The netpoll loop's own halt is a bare `wfi`, not a park, so folding it into the
   same waiter table conflates two different things.

A broadcast is imprecise and cheap; a precise wake was wrong more often than it was
expensive. Recorded in `nic_irq_handler` so it is not re-tried blind.

### 11.5 Four corrections to earlier claims in this document

- **The async-main lap rate is 11,262/s under load and 2,281/s at idle** — not the
  15,412/s quoted in §10 and `MAIN_THREAD_SPLIT.md`. That figure came from
  `poll_calls - poll_progress`, which assumes every `poll()` is a drain lap. It is
  not: `wait_until` polls up to 64x per blocked op, epoll polls, and
  `socket_send`/`socket_recv` each poll once on the way out. Polls are ~10x laps.
  `[NICSTAT] laps=` now reports the real number.
- **The herd-poll rate limit did not "measure worse"** (§10.2). Neither did the
  `pending_removal` GC rate limit. Both comparisons were run-vs-run across
  different boots without normalising for traffic; normalised at ~12,160 rx
  packets/window the GC one shows 6.96 vs 6.87/6.70 polls per packet and 10.7 vs
  11.0/11.5 us per poll — noise. Both were reverted as unearned complexity, not as
  regressions.
- **"Work removed from the lap becomes more laps" is retracted.** It was inferred
  from that same unnormalised comparison and does not survive normalisation.
- **"A `NETWORK` holder sleeps while holding it"** (§9.5) is wrong.
  `smoltcp_net::poll()` wraps the critical section in `PreemptGuard`, which masks
  IRQs, so the holder cannot be descheduled.

### 11.6 GIC audit: no v2/v3 mixing

Checked because misusing the API would explain a lot. It is clean: `gic-v2` is not
a default feature, the v3 driver touches no `GICC` MMIO (system registers
throughout), every dispatch point in `gic.rs` has both arms, and the PE0-hardcoded
`trigger_sgi` is confined to `not(kernel_smp_shared)` at both call sites —
`smp-shared` uses `trigger_sgi_self`.

Two notes, neither a bug today:

- `enable_irq` programs `IROUTER` with **`IRM=0`**, pinning the NIC SPI to aff0=0.
  `IRM=1` ("1-of-N") would let the GIC deliver to any participating PE. Untried;
  it would not fix the waiter-on-another-core problem but would stop core 0
  absorbing every NIC interrupt.
- `trigger_sgi`'s comment claims TargetList bit 0 selects "this CPU". It selects
  PE 0. Harmless where it is reachable (single-core only), latent trap otherwise.

### 11.7 Measurement discipline this section cost us to learn

- **Normalise by traffic.** Poll and lap counts track packet count; comparing raw
  totals between runs invents effects. Three wrong conclusions came from this.
- **Measure a control in the same session.** The same build gave 1,108 and 1,040
  req/s hours apart — ~6 % drift, enough to manufacture a result this size.
- **`bkl-profile` shares are contended *spin* time, not CPU.** 85 % of it is
  `idle` + `irq/sched`, i.e. cores that had nothing to do. Returning a lock sooner
  to an idle core buys nothing. Use it to find *who holds*, never to predict what a
  fix is worth.
- **Counting operations is not measuring them.** `SocketSet::get` is an array
  index; "9M lookups/s" sounded damning and cost nothing.
- **Instrument with O(1) counters.** `iter().count()` per poll made the meter a
  material part of what it measured — ~0.9 us/poll at 128 slots, ~14 us at 2048,
  which inflated the first 2048-slot experiment.

## 12. The `blocking_relax` yield: +30 %, and the SMP=4 wedge it hides (2026-08-20)

Full write-up, including the regression and the method notes, in
[`BLOCKING_RELAX_YIELD_SMP4_REGRESSION.md`](BLOCKING_RELAX_YIELD_SMP4_REGRESSION.md).
Headline, 5 x 2000 requests per arm, all three arms in one session against the
same `httpd`:

| arm | req/s | p50 | p90 | p99 | SMP=4 boot suite |
|---|---:|---:|---:|---:|---|
| §9 baseline (yield + halt) | 1,028 | 601 us | 2,411 us | 4,703 us | 294 passed |
| yield dropped for ALL callers | 1,307 | 502 us | 1,166 us | 3,874 us | **23, wedged** |
| **yield dropped for sockets only** | **1,339** | **482 us** | **967 us** | **3,808 us** | **294 passed** |
| Linux control (2026-08-19) | 1,641 | 576 us | 643 us | 882 us | — |

**p50 is now below Linux** (482 vs 576 us) and p90 is down 60 %. What landed is
`threading::blocking_relax_net` — `blocking_relax` without the leading
`yield_now` — wired into `NetRuntime::blocking_relax` and nowhere else.

### Why it works, and why it must not be kernel-wide

The socket waiter is woken by a *device interrupt*. With the yield, the park is a
scheduler pass + SGI **before** the WFI is entered, so a packet arriving in that
window ends no halt: the waiter reaches WFI just after its own wake and sleeps to
the next tick. Without it the waiter is already halted when the packet lands and
the NIC IRQ ends the halt directly.

The spawn/exec/reap waiters are woken by *another thread on their own core* and
genuinely need the yield. Dropping it for them reintroduces the socket-recv /
`exec_with_io_cwd` cross-core wedge: `SMP=4` freezes after 23 tests, twice,
identically. Single-core cannot see this — the regression test SKIPs at `SMP=1`
and the suite reports a clean 286/0.

### The mechanism is finer-grained parking, NOT less parking

Total parked time per packet is **342 us on both arms**. The parks go 11.6k x
359 us -> 57k x 71 us — same total, 5x finer grain, so a wake lands within it. Every
throughput-side cost moved <10 %. This is a wake-latency fix; the throughput came
free. Full time budget (`scripts/benchmarks/nicstat_breakdown.py`) in the
regression doc; inside `poll()` the stable split is `tx_wait` 31 %, smoltcp 62 %,
`rx_post` 6 %, making `tx_wait` (22.5 us per TX packet) the largest named cost
left and giving §11-item-3's static TX rings a specific thing to beat.

### p99 is now tick-quantised

p90 fell 60 % but p99 barely moved. `TIMER_INTERVAL_US = 3_000` and p99 is 3,808 us
= one tick + ~800 us of real work: a single missed wake on the timer backstop.
`poll max` agrees (3,504-3,698 us). **p99 cannot go below ~3 ms by further wake
tuning** — either close the last lost wake, or shorten the tick, and 3 ms is
already the HVF-safe floor (below ~2.5 ms darwin/arm64 refuses to sleep the vCPU
and burns a saturated host core per guest core).

### Two additions to §11.7's measurement discipline

1. **Back-to-back runs walk into the §11.2 socket-table cliff.** Runs 4-5 of an arm
   scored *half* runs 1-3 with no code change (1,030-1,077 -> 525-537 req/s), with
   the full swallowed-wake signature (polls/pkt 6.4-7.0 -> 4.2, us/poll 11.3-12.2
   -> 16.6-16.8, us/park 236-349 -> 531-585) produced by **run order alone**. Any
   arm measured second would have been condemned. smoltcp holds `TimeWait` for
   `CLOSE_DELAY` = 10 s; a 25 s settle between runs removes it entirely (5 runs
   within 3 %). `run_nic_ab.py --settle` exists for this; do not lower it.
2. **A single-core boot suite does not verify an SMP primitive.** 286/0 was a green
   light on a kernel that freezes at `SMP=4`. Any `blocking_relax` change must be
   run at `SMP=4`, and the acceptance signal is the line
   `smp_shared_blocking_wait_peer_progress PASSED` — not the pass count, and
   emphatically not the `[BKL] stuck` count, which is ~85 on healthy and wedged
   kernels alike.

### `net-noalloc` re-measured on top of §9 + §12 — NEUTRAL, still off

§11-item-3 asked for this: the static RX/TX rings lost on p90 in §7, their
suspected loss mechanism was a wake problem, and §9/§12 fixed wake problems. Same
session, same protocol, 5 x 2000:

| | split, no rings | rings (`net-noalloc`) |
|---|---:|---:|
| req/s median | 1,339.0 | 1,377.2 |
| req/s range | 1,236-1,471 | 1,285-1,427 |
| p50 | 481.7 us | 468.5 us |
| p90 | 967.4 us | 865.9 us |
| p90 range | 734-1,616 | 809-1,156 |
| p99 | 3,807.7 us | 3,816.8 us |

**Indistinguishable.** The ranges overlap heavily and +2.8 % on the median is
inside the ~6 % session drift of §11.7. One soft signal, not claimed: rings' p90
*spread* is tighter (809-1,156 vs 734-1,616), which is the shape you would expect
from removing per-packet allocation.

**§7's "the rings lose on p90" is now stale** — that loss was the wake problem
§9 and §12 fixed. They are no longer a regression. They still should not go
default-on: added machinery for no measured gain. What they are now is *available*
for the pipelined workload (redis) §7 says was never measured.

The mechanistically useful part is the time budget:

| per window, ~12,170 pkts | split | rings |
|---|---:|---:|
| parked | 4,120-4,164 ms | 4,144-4,146 ms |
| in `poll()` | 851-891 ms | 878-891 ms |
| **`tx_wait`** | 273-275 ms, **22.5-22.7 us/pkt** | 276-279 ms, **22.7-23.0 us/pkt** |
| stack/other | 61.6-62.7 % of poll | 62.4 % of poll |

**`tx_wait` per packet does not move.** So the 31 %-of-poll cost is **not**
allocation or copy cost — removing both leaves it identical. It is time waiting on
virtio TX completion: device/host side (QEMU SLIRP), not kernel side. Correct the
expectation accordingly — `tx_wait` is probably not addressable from inside the
kernel, and it should stop being cited as the next kernel-side target.

### Harness added

- `scripts/benchmarks/run_nic_ab.py` — boots one arm, starts `httpd`, runs N x 2000
  with a settle window, slices the `[NICSTAT]` windows per run, prints medians.
  Confirms readiness with a real ssh round trip, because `[herd] Started sshd`
  arrives torn by console interleaving (`[herd] Started [syscall] socket(...)`)
  often enough to cost an arm.
- `scripts/benchmarks/nicstat_breakdown.py` — window counters -> time budget,
  wall-vs-core and nesting handled.
- `bench_nic_rtt.py` now parses the socket-table line (`sockets=N/CAP`,
  `epoch_saves`) and `laps=`, which were previously dropped on the floor.

## Background

- [`BENCHMARK_PERFORMANCE_ATTEMPT_0.md`](BENCHMARK_PERFORMANCE_ATTEMPT_0.md) §4
  derived a fixed round-trip ceiling from redis ops/s and predicted it was "a
  property of the single netpoll drain loop, which nothing has changed". This
  document measures that directly and identifies the mechanism.
- [`SERIAL_TRACE_TRAFFIC_AUDIT.md`](SERIAL_TRACE_TRAFFIC_AUDIT.md) — console
  cost, and the rule to histogram first.
- [`CPU_LOAD_REGRESSION_INVESTIGATION.md`](CPU_LOAD_REGRESSION_INVESTIGATION.md)
  — why the tick is 3 ms and not 1 ms on this host.
