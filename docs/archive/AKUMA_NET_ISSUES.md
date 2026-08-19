# Akuma networking: why a round trip costs milliseconds (2026-08-19)

**Status: measured, root-caused, three fixes landed and A/B-verified.** This is the investigation
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

## 7. Not done

1. **The p90/p99 tail** (§4) — the two candidates named there. This is now the
   whole remaining gap to Linux.
2. **Async TX + static buffer rings** (§3.2, §3.3) — contained inside
   `crates/akuma-net/src/smoltcp_net.rs`; worth ~20-30 us per round trip and
   removes an IRQ-masked spin.
3. **`userspace/httpd` does not run on Linux** — `hello` (the minimal libakuma
   binary) SIGSEGVs too, so the blocker is in libakuma's startup, not httpd.
   Worth fixing: it would give the same-binary A/B that
   `scripts/probes/` provides for `std` binaries. Syscall numbers are already
   Linux aarch64 apart from the 300+ Akuma extensions (`SPAWN` 301, `TIME` 305,
   `UPTIME` 319), and those return `-ENOSYS` rather than faulting.
5. **`connect`-mode comparison against Docker is not meaningful** — Docker
   Desktop's proxy opens a fresh backend connection per inbound connection
   (measured p50 137 us) where QEMU SLIRP does not (Akuma p50 120 us, i.e.
   *faster*). Use `connect` for Akuma-vs-Akuma A/B only.

## Background

- [`BENCHMARK_PERFORMANCE_ATTEMPT_0.md`](BENCHMARK_PERFORMANCE_ATTEMPT_0.md) §4
  derived a fixed round-trip ceiling from redis ops/s and predicted it was "a
  property of the single netpoll drain loop, which nothing has changed". This
  document measures that directly and identifies the mechanism.
- [`SERIAL_TRACE_TRAFFIC_AUDIT.md`](SERIAL_TRACE_TRAFFIC_AUDIT.md) — console
  cost, and the rule to histogram first.
- [`CPU_LOAD_REGRESSION_INVESTIGATION.md`](CPU_LOAD_REGRESSION_INVESTIGATION.md)
  — why the tick is 3 ms and not 1 ms on this host.
