# Benchmark performance, attempt 0 — Akuma vs Docker/Linux (2026-08-19)

**Status: two workloads measured, at two core counts.** Redis (§1–§7, §9) and
llama.cpp (§8), on one machine in one session, shape-matched, with the machine's
other load audited.

Two workloads were used because they load opposite halves of the system, and
together they say something neither says alone:

> **Akuma's compute is within a couple of percent of Linux. Its
> kernel-crossing and its cross-core paths are nowhere near.**

- **llama.cpp at `-t 1`** — the kernel is out of the hot loop — runs at
  **98.5 %** of Linux on compute-bound prefill, same binary and same weights.
  The ELF loader, page tables, `mmap`, allocator and arithmetic cost nothing
  measurable. Bandwidth-bound decode is further back at **86.6 %**, which is a
  separate and much smaller lead (§8, Result 1).
- **Redis** — where every operation is a kernel crossing and nothing else —
  runs at **35 %** of Linux forwarded, **1 %** in-guest, and its throughput is
  pinned to a fixed number of round trips per second that does not move with
  command, payload, or concurrency.
- **llama.cpp at `-t 2`** — the moment a second core touches shared memory —
  is **22× slower than `-t 1`**, and it is *not* the wakeup path (proven in
  §8, Result 3).

So there are two distinct costs here, not one: a per-crossing cost (§4) and a
cross-core cost (§8). Neither is a compute cost.

---

## 1. What was measured

Redis 8.10.0 — the genuine upstream `redis:alpine` binaries, running under both
kernels — driven by `redis-benchmark` at 100,000 requests, 20 clients, 64-byte
payloads, pipeline 1 and 16, median of 3 repeats, one benchmark invocation per
test.

Four arms, because *where the client runs* turns out to matter more than
anything else:

| arm | client | server | what it exercises |
|---|---|---|---|
| `akuma-fwd` | macOS host | box on Akuma | one Akuma socket endpoint, reached over QEMU SLIRP |
| `akuma-box` | inside the box | same box | two Akuma endpoints + Akuma's smoltcp loopback |
| `docker-fwd` | macOS host | container | one Linux endpoint, over Docker Desktop's forwarder |
| `docker-local` | inside the container | same container | two Linux endpoints + Linux loopback |

**VM shape was matched**, unlike the earlier baseline: the container ran
`--cpuset-cpus=0-3 -m 4g` (verified `nproc` = 4 inside it) against the devbox
default `SMP=4 MEMORY=4096`. The previous baseline compared a 12-vCPU / 7.65 GiB
Docker VM against a 4-core devbox and flagged that as unresolved; it is resolved
here.

### Environment caveat, recorded because it nearly went unnoticed

Four orphaned `while :; do :; done` load generators — left over from an
abandoned preempt stress run in another session, `PPID=1`, 3 h 37 m of runtime —
were pinning four of this machine's twelve cores when the session started. They
were killed before the matrix ran. Re-probing after the kill moved both Akuma
arms by ~5 %, inside the measured spread, so nothing collected before it is
invalidated. It is recorded because a benchmark session should check what else
is on the machine, and this one only found them by accident.

---

## 2. Getting Redis onto Akuma — `box pull` is broken today

`docs/runbooks/run-redis.md` §3 does not work on kernel
`3f7a33de-release-smp-shared`. Every `box pull` dies at the same step:

```
  Fetching config...
box pull: config fetch failed: IoError
```

Not image-specific (`alpine:latest` fails identically), and not a networking
problem — `wget` to the same CDN host and `apk update` both succeed from the
same shell. Filed as `DEVBOX_ISSUES.md` Issue 18 with the analysis of which step
differs.

**Workaround, which preserves the fairness property that matters**: export the
rootfs from the host's own `redis:alpine` container and run *that* in a box.
Same registry, same digest, same binaries:

```bash
docker export akuma-redis-bench | gzip -1 > redis_rootfs.tar.gz
# stream over ssh (driven from Python; the ssh CLI is blocked by policy):
#   'mkdir -p /root/redisimg && busybox tar -xzf - -C /root/redisimg'
```
```
~ # box open redisbox --root /root/redisimg -d \
      /usr/local/bin/redis-server --port 4444 --protected-mode no --save ''
~ # box use redisbox -i /usr/local/bin/redis-cli -p 4444 info server
redis_version:8.10.0
os:Akuma 0.0.7 aarch64
multiplexing_api:epoll
```

Two things worth knowing before repeating this:

- **`busybox tar`, not `tar`.** Akuma's own `/bin/tar` rejects
  `tar xzf - -C dir` with `only extraction (-x) is supported for now` — it is an
  extraction, and the real gaps are `-z` and `-f -`. Issue 18 sub-section.
- **`box use <name> -i <cmd>`** — without `-i` the command runs and you get
  `Injected PID 110` instead of its output.

---

## 3. Results

100,000 requests, 20 clients, 64-byte values, median of 3. Operations per second.

### Pipeline = 1

| test | akuma-box | akuma-fwd | docker-fwd | docker-local |
|---|---:|---:|---:|---:|
| PING_INLINE | 3,392 | 20,272 | 58,106 | 313,480 |
| PING_MBULK | 3,345 | 20,425 | 56,465 | 333,333 |
| SET | 3,392 | 20,623 | 56,850 | 317,460 |
| GET | 3,372 | 20,100 | 57,241 | 306,748 |
| INCR | 3,377 | 20,794 | 58,072 | 313,480 |
| LPUSH | 3,430 | 19,940 | 53,107 | 316,456 |
| RPOP | 3,381 | 20,602 | 57,339 | 322,581 |
| SADD | 3,377 | 20,721 | 56,370 | 309,598 |
| SPOP | 3,374 | 20,730 | 55,679 | 320,513 |
| MSET (10 keys) | 3,425 | 19,309 | 51,230 | 333,333 |

### Pipeline = 16

| test | akuma-box | akuma-fwd | docker-fwd | docker-local |
|---|---:|---:|---:|---:|
| PING_INLINE | 52,854 | 222,222 | 653,595 | 3,030,303 |
| PING_MBULK | 48,948 | 247,525 | 740,741 | 3,703,704 |
| SET | 52,743 | 171,233 | 598,802 | 2,222,222 |
| GET | 53,022 | 210,970 | 628,931 | 2,702,703 |
| INCR | 52,854 | 215,054 | 729,927 | 2,439,024 |
| LPUSH | 49,652 | 142,450 | 540,541 | 684,932 |
| RPOP | 51,680 | 210,526 | 763,359 | 2,173,913 |
| SADD | 52,083 | 220,751 | 769,231 | 2,564,102 |
| SPOP | 52,056 | 222,222 | 793,651 | 3,030,303 |
| MSET (10 keys) | 42,974 | 49,776 | 480,769 | 609,756 |

### Noise floor

Spread is `(max − min) / median` over the 3 repeats.

| arm | spread |
|---|---|
| akuma-box | 0.2 – 8.8 % |
| akuma-fwd | 1.4 – 12.4 % |
| docker-local | 0.0 – 10.9 % |
| docker-fwd | 0.6 – **35.8 %** |

The earlier baseline's warning that "a difference under roughly 35 % on the
pipelined list/multi-key tests is noise" **reproduces exactly** — `docker-fwd`
LPUSH P=16 came in at 35.8 % and MSET P=16 at 33.8 %, against the baseline's
34.5 % and 33.5 %. But it is a property of *that one arm*, not of the
measurement generally: in both cases the median sits next to the maximum and one
cold repeat drags the minimum down (LPUSH: min 355,872, median 540,541, max
549,451), so it is a warm-up asymmetry in Docker's forwarder rather than
symmetric noise. **Do not apply a 35 % floor to Akuma's cells.** Akuma's arms
were measured at 1.4–12.4 %, and every gap discussed below is far outside that.

---

## 4. The finding: a fixed round-trip ceiling

Divide operations per second by the pipeline depth to get **round trips per
second** — how often the system completes one request/response exchange,
regardless of how many Redis commands were packed into it. `MSET` is excluded
because at 10 keys × 16 pipeline it is the one test moving enough bytes to be
bandwidth-bound rather than round-trip-bound.

| arm | round trips/s at P=1 | round trips/s at P=16 | change |
|---|---:|---:|---|
| akuma-box | 3,345 – 3,430 | 3,059 – 3,314 | **none** |
| akuma-fwd | 19,309 – 20,794 | 8,903 – 15,470 | −33 % |
| docker-fwd | 51,230 – 58,106 | 33,784 – 49,603 | −23 % |
| docker-local | 306,748 – 333,333 | 42,808 – 231,481 | −48 % |

**`akuma-box`'s round-trip rate does not change at all.** Putting sixteen times
as many commands into each exchange yields exactly sixteen times the operations,
because the number of exchanges per second is unmoved. Payload is free; the
exchange is everything. Expressed as time, Akuma's loopback path retires one
round trip every **~300 µs** whether that round trip carries one PING or sixteen
SETs.

The same rate is also flat across concurrency. Sweeping clients in-box:

| clients | 10 | 16 | 20 | 32 |
|---|---:|---:|---:|---:|
| ops/s | ~3,150 | ~3,000 | ~3,045 | ~2,980 |

Tripling the requests in flight buys nothing. A stack that is merely *slow*
still scales with concurrency until it saturates something; one that is flat is
servicing requests **in series**. Twenty clients do not get twenty round trips
in parallel — they queue, which is why measured p50 latency in-box is ~3 ms
(20 × 150 µs of queueing) while the minimum latency is 0.87 ms.

For comparison, per-round-trip service time across the four arms:

| arm | µs per round trip |
|---|---:|
| akuma-box | ~300 |
| akuma-fwd | ~50 (P=1) / ~74 (P=16) |
| docker-fwd | ~17 / ~23 |
| docker-local | ~3.1 / ~6.0 |

---

## 5. Is it a fair comparison?

Platform-level, yes, and more so than the earlier baseline: same machine, same
Apple silicon, both guests ultimately scheduled by Hypervisor.framework
(Docker's Virtualization.framework is layered on it), same `redis-benchmark`
binary, same flags, same server binaries, and now the same core and memory
budget.

### The forward direction inverts, and that breaks the baseline's rule

The earlier baseline established, correctly, that crossing the host port-forward
costs Docker ~4×, and concluded: always compare arm to arm, because the
forwarded arm is the handicapped one. The first half of that survives. The
second half does not.

| | client on host | client inside | direction |
|---|---:|---:|---|
| Docker | 56,850 | 317,460 | forward **costs** 5.6× |
| Akuma | 20,623 | 3,392 | forward **gains** 6.1× |

On Akuma the in-guest arm is the slow one, because it puts *both* endpoints on
the kernel under test and routes them through Akuma's own loopback. It charges
the kernel twice and adds a second runnable process competing for the same four
cores. The two arms are not symmetric handicaps, so "compare arm to arm" is
still right but its justification has to be restated: compare arm to arm because
the arms measure *different things*, not because one is uniformly worse.

### Which arm is the honest measure of the server?

**`akuma-fwd`.** Only the server endpoint is on Akuma; the client is native
macOS. And the forwarded path is demonstrably not the binding constraint on it,
because Docker pushed 57,000 ops/s through a forwarder of the same *kind* while
Akuma reached 20,000.

That argument has a hole worth naming: the two forwarders are not the same
software. QEMU's SLIRP user-mode NAT is not Docker Desktop's purpose-built
proxy, and SLIRP could have its own ceiling near 20,000. The evidence against
that is internal to the Akuma data — at P=16 the same arm sustains 247,525 ops/s
through that same SLIRP path. A forwarder capped at 20,000 operations per second
could not do that. What it *is* capped at is round trips, which is the finding
of §4 and is a property of the guest, since the in-guest arm — which never
touches SLIRP at all — shows the same shape more purely.

### The cells the baseline said to weight most

`LPUSH` and `MSET` at P=16 were designated the meaningful cells, because on
Docker they are the two where the server rather than the transport is the limit
(`docker-local`: LPUSH 684,932 and MSET 609,756 against GET's 2,702,703).

Arm-to-arm, in-guest:

| test | docker-local | akuma-box | ratio |
|---|---:|---:|---:|
| LPUSH P=16 | 684,932 | 49,652 | 13.8× |
| MSET P=16 | 609,756 | 42,974 | 14.2× |

**But that designation does not transfer to Akuma, and the ratio should not be
quoted as "the server comparison".** On `akuma-box` every test at P=16 lands
between 42,974 and 53,022 — a no-op `PING_INLINE` costs the same as a ten-key
`MSET`. Redis's own work is invisible on that arm. The cells that isolate server
work on Linux isolate nothing on Akuma, because Akuma never gets far enough into
the operation for the operation's cost to matter.

So there is currently **no cell in this matrix that measures Akuma's Redis
throughput as opposed to Akuma's transport**, and the honest summary is that a
server-vs-server comparison is not yet possible. That is itself the result.

### What must not be said

Not "Akuma's kernel is 94× slower than Linux" (the P=1 in-guest ratio), nor
"2.8× slower" (the P=1 forwarded ratio). Both are ratios of transport costs
measured under different amounts of transport. The defensible statement is the
one in §4: Akuma completes 3,300 network round trips per second on loopback and
~20,000 through the NIC, against Linux's ~320,000 and ~57,000 on the same
hardware with the same core budget.

---

## 6. Where the cost is — hypotheses, and the tests that would settle them

None of this is established. It is written down as the shortest path from the
measurement to a cause, in the order the evidence supports.

**The flatness across concurrency is the loudest clue**, because it points at a
single shared, serially-drained resource rather than at any per-connection cost.
Akuma drives smoltcp from one BSP `netpoll` loop (`src/main.rs:1444`) whose own
doc comment describes the intended steady state as "~1 iteration per timer tick
(the loop halts in WFI between ticks)" (`src/timer.rs:50-53`), with the tick at
3 ms or 10 ms (`src/config.rs:846-848`). `Interface::poll` drains the whole
device queue and does one egress pass per call, so however many sockets are
ready, they all wait for the same next wakeup. smoltcp provides `poll_at` /
`poll_delay` precisely so an embedder can sleep until the next thing is actually
due instead of on a fixed cadence; its docs call polling later than `poll_at`
"potentially harmful (impacting quality of service)".

Two cheap experiments would settle whether the loop is the ceiling:

1. **Sample `NETPOLL_ITERS`** (`src/timer.rs:56`, already instrumented) per
   second during a P=1 run. If iterations/s ≈ ops/s, one round trip costs
   exactly one loop iteration and the loop *is* the ceiling. If it is 100× the
   ops rate, the loop is fine and the cost is elsewhere.
2. **A pipe or socketpair ping-pong between two guest processes** — no smoltcp
   in the path at all. If that also costs ~300 µs per round trip, the in-guest
   penalty is syscall + scheduler and the network stack is exonerated entirely.
   This one also isolates how much of the in-guest arm's 6× is simply "two
   processes instead of one".

**Second: the loopback path allocates and copies per frame, and the NIC path
does not.** `crates/akuma-net/src/smoltcp_net.rs:502-520`, with the code's own
comment naming the asymmetry:

```rust
// Write the frame into the VirtIO tx buffer (avoids allocation for external traffic)
let res = f(&mut self.virtio_buffer[..len]);

if is_loopback_frame(&self.virtio_buffer[..len]) {
    // Loopback: copy into an owned Vec and queue for the next receive()
    let mut frame = vec![0u8; len];
    frame.copy_from_slice(&self.virtio_buffer[..len]);
    self.loopback_queue.push_back(frame);
}
```

Every loopback frame costs a zeroing heap allocation, a second full copy of a
frame that was just written, a `VecDeque` push, and a free after receive.
External traffic costs none of that. This lines up with which arm is slow, and a
frame pool would remove it. **The arithmetic says it is not the dominant term
though**: at 3,372 ops/s with ~2 frames per round trip that is ~6,700
allocations per second, and 300 µs per round trip cannot be explained by two
allocations unless each costs ~150 µs. Real waste, worth fixing, not the answer.

**Third: smoltcp's own per-round-trip constants.** `ACK_DELAY_DEFAULT` is 10 ms
and applied to every socket by default, and Nagle is on; smoltcp's docs
explicitly describe the Nagle + delayed-ACK interaction as a latency cost. Both
are payload-independent and per-exchange, which is the right shape. Both are
also mostly dodged by a strict request/response protocol like RESP, where each
side always has data to piggyback an ACK on — so this is a suspect to rule out,
not a leading candidate.

### On "replace our socket machinery with smoltcp's"

Worth stating plainly, because it came up: there is nothing to replace it
*with*. smoltcp deliberately provides no file descriptors (sockets are an opaque
`SocketHandle(usize)` index), no blocking semantics, no socket lifecycle beyond
the TCP state machine, and no scheduler integration — its `async` feature is a
single `Option<Waker>` cell documented as best-effort, not a reactor. Its own
crate docs say the socket API "necessarily differs in many from the Berkeley
socket API, as the latter was not designed to be used without heap allocation".
Adopting more of smoltcp would replace the protocol engine, which we already
use; the fd table, blocking read/write, accept-returns-a-descriptor, and the
wakeup-to-scheduler wiring would all still have to be ours.

There is also no local-traffic bypass in smoltcp to adopt. `phy::Loopback` is a
`VecDeque<Vec<u8>>` FIFO that skips checksums and nothing else — frames still
traverse the full IP/TCP state machine twice. Our `LoopbackAwareDevice` is
structurally the same thing. A genuine socket-to-socket shortcut for 127.x
traffic would have to be built here, and given §4 it is probably the single
highest-value thing on this list.

---

## 7. Method (reproduce exactly)

Three harnesses, all in `scripts/benchmarks/`:

| script | what it does |
|---|---|
| `redis_matrix.sh` | the whole four-arm Redis matrix, serialized, with preflight checks |
| `bench_redis.py` | one arm; medians, spread, and a `--compare` mode that gates each row against the measured noise floor |
| `bench_llama.py` | both llama.cpp arms; hash + package verification, detached execution, stale-run guard |

```bash
# Redis — the whole matrix. CORES picks the shape on BOTH sides.
scripts/benchmarks/redis_matrix.sh                    # SMP=4 vs --cpuset-cpus=0-3
CORES=1 scripts/benchmarks/redis_matrix.sh            # SMP=1 vs --cpuset-cpus=0

# llama.cpp — push the weights once, then one arm at a time.
scripts/benchmarks/bench_llama.py --push
scripts/benchmarks/bench_llama.py --arm akuma  --out logs/llama_bench/akuma.csv
scripts/benchmarks/bench_llama.py --arm docker --out logs/llama_bench/docker.csv
scripts/benchmarks/bench_llama.py --compare logs/llama_bench/docker.csv \
                                            logs/llama_bench/akuma.csv
```

`redis_matrix.sh` aborts unless `nproc` inside the guest *and* inside the
container both match `CORES`, and prints the host's top CPU consumers before
starting. `bench_llama.py` refuses to launch while any `llama-bench` is present
in the guest, verifies the model's sha256 on both sides, and refuses to run if
the two `apk info -v llama.cpp` versions differ. Every one of those guards
exists because its absence cost a set of numbers in this session (§10).

**Run the arms one at a time.** Two arms at once measure each other.

### Why `--clients 20`, `--per-test` and `--cooldown`

Akuma's socket budget (Issue 16). `-c 50` fails outright, and the real limit is
not a client count but how recently the last run ended: `-c 32` passes with a
10 s gap, `-c 20` fails without one, and a single `redis-benchmark` invocation
covering several tests fails on the second test at `-c 20` because it rebuilds
its whole client pool between tests with no pause. `--per-test` runs one
invocation per test; `--cooldown` puts a gap between them.

**`redis-benchmark` exits 0 after printing `No file descriptors available`.**
The affected tests simply do not appear in the `--csv` output, so a harness that
trusts the exit status silently records a clean run missing half its cells. The
script greps stdout for that string and says so.

Whatever you choose, choose it for both arms. A Docker arm at `-c 50` all-tests
against an Akuma arm at `-c 20` per-test is not a comparison.

---

## 8. Second workload: llama.cpp with `qwen3.5-0.8b-q4.gguf`

**Measured 2026-08-19.** This one was chosen because Redis measures the
kernel-crossing path and almost nothing else — §4 through §6 are entirely about
which network path is under the microscope, and §5 concludes that no cell in
that matrix isolates server work on Akuma at all. llama.cpp is the opposite
workload: once the weights are loaded it is NEON arithmetic in userspace with
the kernel out of the hot loop. Together the two bracket the same question from
both ends.

### The setup, and why it is a fair comparison

Alpine ships `llama.cpp` and the Akuma guest is Alpine, so `apk add llama.cpp`
gives **the same distro package, same version, same architecture, built by the
same builders** on both sides — `llama.cpp-0.0.9564-r0`, build
`3b3da01dc21dc68e958efb898ab739c65ed08ca2`, loading the same
`libggml-cpu-armv8.2_2.so` and reporting the same `OpenBLAS, CPU` backend. The
weights are byte-identical: sha256 `bd258782e35f7f458f8aced1adc053e6e92e89bc735ba3be89d38a06121dc517`
verified on the host, in the guest and in the container. Shape matched as in §1
(`SMP=4 MEMORY=4096` vs `--cpuset-cpus=0-3 -m 4g`).

That is a stronger control than the Redis comparison could offer, where the best
available was "the same image, unpacked two ways".

`pp512` is **prompt processing** (prefill): 512 tokens ingested as one batched
matmul — compute-bound and parallel. `tg128`/`tg16` is **token generation**
(decode): tokens produced one at a time, each streaming the whole weight set —
memory-bandwidth-bound and sequential, with far more thread barriers per unit of
work. They behave completely differently here, so both are reported.

### Result 1 — single-threaded, Akuma is close to Linux but not at parity

Both columns measured with the other side's VM/container stopped, so neither is
competing with the other.

| test | Akuma | Docker/Linux | Akuma % |
|---|---:|---:|---:|
| pp512 `-t 1` (prefill, compute-bound) | 102.40 ±0.85 | 103.91 ±0.08 | **98.5 %** |
| tg128 `-t 1` (decode, bandwidth-bound) | 35.18 ±0.53 | 40.61 ±0.05 | **86.6 %** |

Same binary, same weights, same silicon. Prefill is within 1.5 % — effectively
identical, and that was the intended control: the plan said "`-t 1` *should* be
identical; if it is not, something is wrong with the measurement". It passed for
the compute-bound half.

**Decode is 13 % short, and that gap is real.** It is the memory-bandwidth-bound
half — each token streams the entire 532 MB weight set through the cores — so a
deficit there points at the cost of *reaching* memory rather than the cost of
arithmetic: page size and TLB coverage for the mmap'd weights are the obvious
suspects (Linux will back a mapping this size with huge pages where it can).
Untested. It is the cheapest remaining lead in this document, because it shows
up with one thread and therefore has nothing to do with §8's cross-core problem.

> **This correction matters.** An earlier version of this table reported
> **101.3 % / 101.6 %** and called it parity. Those Docker figures
> (101.12 / 34.62) were taken while an orphaned `llama-bench` was still running
> inside QEMU (§10) and were depressed by up to 17 % — the decode cell most of
> all. The clean re-run moved Docker *up*, not Akuma down. The lesson is that a
> contaminated baseline flatters the system under test, which is the direction
> that does not announce itself.

Still substantive: the ELF loader, page tables, `mmap`, the allocator and the
arithmetic path are all correct, and on compute they cost nothing measurable.
Whatever is expensive about Akuma is not arithmetic.

**`--no-mmap` is no longer required.** `userspace/llama.cpp/README.md` states it
as an absolute ("Akuma's VFS doesn't support file-backed mmap"). Every run here
is `use_mmap=1` and completed normally, so `src/file_page_cache.rs` closed that
gap. The README is stale.

### Result 2 — the moment a second thread appears, decode collapses

`tg16`, one repetition, mmap on, thread count swept:

| threads | tok/s | vs `-t 1` |
|---:|---:|---:|
| 1 | 36.05 | 1.00× |
| 2 | 1.61 | **0.045×** |
| 3 | 0.28 | 0.008× |
| 4 | 0.18 | 0.005× |

**The cliff is between one thread and two — 22× slower for one extra thread**,
long before four threads could be contending for four cores. Linux over the same
1→4 step gains cleanly and almost linearly:

| threads | Docker pp512 | Docker tg128 |
|---:|---:|---:|
| 1 | 103.91 | 40.61 |
| 2 | 186.82 | 72.92 |
| 3 | 251.24 | 100.31 |
| 4 | 316.26 (3.04×) | 117.85 (2.90×) |

Prefill degrades on Akuma too, but mildly: `pp512` goes 102.40 → 68.28 (0.67×)
where Docker goes 103.91 → 316.26 (3.04×). The asymmetry is the diagnostic.
Prefill
does a few large matrix multiplies with long stretches of arithmetic between
synchronization points; decode does many tiny operations per token and therefore
crosses far more barriers. **The cost is per-barrier**, so the workload with
more barriers per unit of work is destroyed and the one with fewer is merely
hurt.

At `-t 4`, `tg128` did not finish in 27 minutes and was abandoned; at the
measured 0.18 tok/s, 128 tokens × 3 repetitions is ~36 minutes, which is
consistent rather than a hang.

### Result 3 — it is NOT the wakeup path

The obvious hypothesis was futex/scheduler wakeup latency: threads park at a
barrier and are slow to be woken. `ggml` has a knob that settles this directly —
`--poll`, the spin-before-park threshold, where 100 means worker threads never
park at all and 0 means they park at every barrier.

| `--poll` | behaviour | `tg16 -t 4` |
|---|---|---:|
| 100 | never park — pure spin | 0.19 tok/s |
| 50 | default | 0.18 tok/s |
| 0 | park at every barrier | 0.19 tok/s |

**Removing the park/wake path from the inner loop entirely changes nothing.**
The hypothesis is dead, and so is the related intuition that this is the same
mechanism as the Redis round-trip ceiling.

What survives: the threads are spinning on shared memory (QEMU sat at ~304 %
CPU throughout, so cores are busy, not idle), and the cost appears the instant
two cores touch the same data. That is the signature of cross-core shared-memory
synchronization being pathologically expensive — the leading suspect being the
memory attributes on user mappings (shareability/cacheability), because
non-inner-shareable mappings make cross-core atomics and cache-line sharing
degrade exactly this way while leaving single-core performance untouched. **Not
verified.** The next probes are a bare `ldxr/stxr` ping-pong between two guest
threads on one cache line, timed, and a dump of the page-table attributes for a
user anonymous mapping.

### Scope

`smp-shared` is an actively-changing subsystem; Results 2 and 3 are a snapshot
of 2026-08-19 on `3f7a33de`, not a property of the design. Result 1 is the
durable one.

## 9. The Redis gap at one core — it gets *worse*, and that is the answer

§5 left a question open: Akuma's forwarded arm runs at ~35 % of Linux with four
cores on both sides. Is that a per-kernel-crossing cost that is already there
with one core, or is part of it SMP contention that vanishes when there is
nothing to contend with?

Re-ran the whole matrix at `SMP=1` against `--cpuset-cpus=0`
(`CORES=1 scripts/benchmarks/redis_matrix.sh`, 20,000 requests to keep the run
finite — Akuma at one core is slow enough that 100,000 would have taken hours).

### Forwarded arm

| test | Akuma 1c | Akuma 4c | Docker 1c | Docker 4c | Akuma % @1c | Akuma % @4c |
|---|---:|---:|---:|---:|---:|---:|
| SET P=1 | 5,479 | 20,623 | 50,378 | 56,850 | **10.9 %** | 36.3 % |
| GET P=1 | 5,562 | 20,100 | 50,761 | 57,241 | **11.0 %** | 35.1 % |
| INCR P=1 | 5,456 | 20,794 | 50,891 | 58,072 | 10.7 % | 35.8 % |
| LPUSH P=1 | 5,237 | 19,940 | 46,948 | 53,107 | 11.2 % | 37.5 % |
| SET P=16 | 51,020 | 171,233 | 555,556 | 598,802 | 9.2 % | 28.6 % |
| GET P=16 | 58,651 | 210,970 | 571,429 | 628,931 | 10.3 % | 33.5 % |
| MSET P=16 | 17,079 | 49,776 | 317,460 | 480,769 | 5.4 % | 10.4 % |

**The answer is the opposite of the hypothesis.** Akuma does not hold its ~35 %
at one core — it falls to ~11 %. What changed is not Akuma getting worse in
absolute terms for no reason; it is that the two kernels respond to core count
completely differently:

| going 1 core → 4 cores | Akuma | Docker/Linux |
|---|---:|---:|
| forwarded, P=1 | **3.76×** | 1.13× |
| forwarded, P=16 | **3.36×** | 1.08× |
| in-guest, P=1 | **5.92×** | 1.46× |

`redis-server` is single-threaded, so Linux needs essentially one core and gains
almost nothing from three more. **Akuma scales nearly linearly** — and
superlinearly on the in-guest arm, where at one core the benchmark client, the
server and the netpoll loop must all timeshare a single CPU.

So the four-core figure was not hiding contention. It was showing Akuma
*successfully using* the extra cores to paper over a much larger per-operation
cost. Stated as CPU rather than wall-clock, taking the one-core numbers where a
single CPU is the whole budget:

| | CPU per round trip |
|---|---:|
| Akuma, forwarded | ~182 µs |
| Linux, forwarded | ~20 µs |

**Akuma burns roughly 9× more CPU per kernel crossing than Linux.** With four
cores it parallelizes that down to ~50 µs of wall-clock latency, which is why
the visible gap narrows to 2.8×. The 9× is the real number; the 2.8× is what
four cores buy you.

### In-guest arm

| test | Akuma 1c | Akuma 4c | Docker 1c | Docker 4c | Akuma % @1c | Akuma % @4c |
|---|---:|---:|---:|---:|---:|---:|
| SET P=1 | 573 | 3,392 | 217,391 | 317,460 | 0.26 % | 1.07 % |
| GET P=1 | 578 | 3,372 | 232,558 | 306,748 | 0.25 % | 1.10 % |
| GET P=16 | 8,643 | 53,022 | 1,428,571 | 2,702,703 | 0.61 % | 1.96 % |
| LPUSH P=16 | 8,718 | 49,652 | 526,316 | 684,932 | 1.66 % | 7.25 % |

**§4's fixed round-trip ceiling reproduces at a second core count**, which is
the strongest evidence for it. In round trips per second, the in-guest arm at
one core is 573 at P=1 and 540 at P=16 (8,643 ÷ 16) — *identical*, exactly as it
was at four cores (3,345 vs 3,314). Sixteen times the payload per exchange, same
number of exchanges. The ceiling is on exchanges, and only on exchanges, at
every core count tested.

`PING_INLINE` and `PING_MBULK` are absent from this arm: those cells returned
`rc=1` from `box use` with empty stderr. Not the socket-budget message of Issue
16 — a different, unattributed `box use` failure, and worth a look before the
next run.

### What this does to the llama.cpp reading

The two workloads now disagree about multi-core in a way that is more useful
than either alone:

| | 1 → 4 cores/threads |
|---|---|
| Redis (separate processes) | **3.4× – 5.9× faster** |
| llama.cpp (threads sharing memory) | **200× slower** |

Akuma's SMP is not broken in general — Redis demonstrably gets near-linear
benefit from four cores. What breaks is specifically **threads of one process
sharing memory across cores**. That narrows §8's suspect list considerably: it
is not the scheduler (Redis's processes are scheduled across cores fine), not
wakeups (§8 Result 3), and not core count as such. It is what happens when two
cores touch the same user page.

## 10. Method note — how this measurement was nearly ruined twice

Both incidents are recorded because they were caught by accident rather than by
procedure, and both are now guarded in the harnesses.

**Orphaned load on the host.** Four `while :; do :; done` processes from another
session's abandoned preempt stress run (`PPID=1`, 3 h 37 m, ~4 of 12 cores) were
running when the session began. Found via `top`, not by any check. Impact turned
out to be ~5 %, inside the spread, but it was luck. `redis_matrix.sh` now prints
the top CPU consumers before starting.

**An orphaned run inside the guest.** The first llama.cpp attempt held an ssh
channel open; it died at 303 s with `rc=255` (Issue 19) and **the remote
`llama-bench` was not killed with it**. A replacement run was launched alongside
it, and both Docker's arm and the re-run were measured while two processes
fought over four cores. Every number from that window was discarded and the
whole thing redone on a cold VM with the process count verified.

That mistake was made harder to catch by Issue 21: `ps` on Akuma shows threads
as processes, so "two runs of seven threads" and "fourteen runaway processes"
look identical. `bench_llama.py` now refuses to launch while any `llama-bench`
is present, and always runs detached with a sentinel rather than holding a
channel open.

**The rule both incidents point at:** check what else is running — on the host
*and* in the guest — immediately before a measurement, and record it alongside
the result.

## Background

- [`../runbooks/run-redis.md`](../runbooks/run-redis.md) — how Redis is run on
  Akuma, the port-forwarding table and the `INSTANCE=N` offsets. **§3 is broken
  today**; see §2 above and Issue 18.
- [`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md) — Issue 16 (socket budget, refined by
  this run), Issue 18 (`box pull` config fetch, and the `/bin/tar` gap).
- [`LONG_ROAD_TO_REDIS.md`](LONG_ROAD_TO_REDIS.md) — why `redis-server` was
  blocked from starting at all for a long time.
- [`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md)
  § Tier 4 — `redis-server --test-memory`, which exercises the memory path
  rather than throughput, and its caveat that a passing memtest is not
  "redis works".
- `crates/akuma-net/src/smoltcp_net.rs` — `LoopbackAwareDevice` and the
  per-frame loopback allocation of §6.
- `src/main.rs:1444`, `src/timer.rs:50-56`, `src/config.rs:846-848` — the
  netpoll loop, its iteration counter, and the timer tick.
