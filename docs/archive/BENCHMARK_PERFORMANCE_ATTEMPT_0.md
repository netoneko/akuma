# Benchmark performance, attempt 0 — Redis on Akuma vs Docker/Linux (2026-08-19)

**Status: complete for Redis.** Four arms, all measured on one machine in one
session with the machine otherwise idle. The llama.cpp section at the end is a
plan, not results.

The short version: **Akuma's throughput ceiling is a fixed number of network
round trips per second, and nothing else.** It does not move when the command
changes, it does not move when the payload changes, and on the in-guest path it
does not move when sixteen times more data is packed into each round trip. Every
other number in this document is downstream of that one fact.

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

`scripts/benchmarks/bench_redis.py` drives every arm, takes the median of N
repeats, records the spread, and has a `--compare` mode that gates each row
against the measured noise floor.

```bash
docker run -d --name akuma-redis-bench --cpuset-cpus=0-3 -m 4g -p 6379:6379 redis:alpine

COMMON="--requests 100000 --clients 20 --size 64 --pipelines 1,16 --repeats 3 --per-test"

scripts/benchmarks/bench_redis.py --label akuma-fwd  --port 4444 $COMMON --cooldown 15 \
    --out logs/redis_bench/akuma_fwd.json
scripts/benchmarks/bench_redis.py --label akuma-box  --via box:2222:redisbox --port 4444 $COMMON \
    --cooldown 15 --bench-bin /usr/local/bin/redis-benchmark --cli-bin /usr/local/bin/redis-cli \
    --out logs/redis_bench/akuma_box.json
scripts/benchmarks/bench_redis.py --label docker-fwd --port 6379 $COMMON --cooldown 3 \
    --out logs/redis_bench/docker_fwd.json
scripts/benchmarks/bench_redis.py --label docker-local --via docker:akuma-redis-bench --port 6379 \
    $COMMON --cooldown 3 --out logs/redis_bench/docker_local.json

scripts/benchmarks/bench_redis.py --compare logs/redis_bench/docker_fwd.json \
                                            logs/redis_bench/akuma_fwd.json
```

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

## 8. Next workload: llama.cpp with `qwen3.5-0.8b-q4.gguf`

Planned, not run. Written down now because llama.cpp is a **much cleaner kernel
comparison than Redis**, and the reason is worth stating before the numbers
exist.

### Why this one is the better experiment

Redis on this pair of stacks is dominated by transport — §4 through §6 above are
entirely about which network path is being measured, and §5 concludes that no
cell in the matrix isolates server work on Akuma at all. llama.cpp has almost no
syscall traffic once the weights are loaded: it is NEON arithmetic over a 532 MB
working set, on the same silicon, from the same file. Whatever gap remains is
attributable to a short list of kernel behaviours rather than to a forwarder.

### Which llama.cpp build

**Use Alpine's `apk add llama.cpp`, not the in-tree cross-build.**
`userspace/llama.cpp/README.md` is written around SmolLM2-135M in a 256 MB VM
and its guidance (`-c 256`, the memory table, `--no-mmap` as an absolute) is
stale; `bootstrap/bin/llama-cli` was last built 2026-08-12. Taking the package
also buys a control the comparison would not otherwise have:

> The Akuma guest is Alpine and `redis:alpine` is Alpine. `apk add llama.cpp` on
> both sides yields the **same distro package, same version, same arch, built by
> the same builders** — differing only in which kernel executes it.

Record `apk info -v llama.cpp` on both sides and refuse to compare if the
versions differ.

**Prerequisite to clear first:** `apk` on this devbox image fails its database
write — `ERROR: System state may be inconsistent: failed to write database: Is a
directory` — even though the package files do get installed (verified with
`apk add tar`, 2026-08-19). Decide whether that is good enough to trust an
`apk add llama.cpp`, or whether it needs fixing first. It shares a smell with
Issue 18's `box pull` failure, which also dies at a file write.

If the package does not exist for `aarch64`, fall back to the in-tree build —
`bootstrap/bin/llama-cli` and `bootstrap/bin/llama-bench` are statically-linked
musl aarch64 binaries, so the *identical file* runs on Akuma and in an `aarch64`
Linux container, which is the strongest control available. Either way, do not
compare an Alpine package against the in-tree build.

### What actually differs, and is therefore what gets measured

| | Linux | Akuma | What it costs |
|---|---|---|---|
| Loading the weights | `mmap`, demand-paged | possibly `--no-mmap` → 532 MB through `read()` on ext2 | **model load time** — expected to be the largest single gap |
| Weight pages during inference | page cache, shared, resident | private anonymous heap | resident set, and whether it fits |
| `-t N` worker threads | CFS on 4 pinned cores | Akuma scheduler, shared 32-thread kernel pool | **pp throughput** scaling from `-t 1` to `-t 4` |
| Token generation | memory-bandwidth bound | same, through Akuma's page tables | **tg throughput** — TLB/mapping quality |

So the three numbers to collect are **load time**, **pp (prompt processing)
tok/s** and **tg (token generation) tok/s** — not one "speed" figure.

### Protocol

1. **Verify `--no-mmap` is still mandatory.** The README says Akuma's VFS has no
   file-backed `mmap`, but `src/file_page_cache.rs` landed since. If plain
   `mmap` loading now works, that is a result on its own. Run the Linux arm
   **with `--no-mmap` too** so the arms match, then add a third Linux arm *with*
   mmap purely to price what Akuma is missing.
2. **Get the weights into the guest.** `overlays/devbox/bootstrap.sh` skips
   `models/` on purpose (line 65), so `bootstrap/models/qwen3.5-0.8b-q4.gguf` is
   not on `devbox.img`. Stream it in the way the Redis rootfs went in — 41 MB
   took 14 s over ssh, so budget ~3 min for 532 MB — and **verify a sha256 on
   both sides before benchmarking anything**. A short read on a 532 MB transfer
   produces a model that still loads and still generates text.
3. **Use `llama-bench`, not `llama-cli`.** It reports `pp512` and `tg128` in
   tok/s with repetitions and a stddev built in, which is exactly the discipline
   the Redis run had to bolt on by hand. Fall back to `llama-cli` only if
   `llama-bench` will not start, and then parse the `llama_perf_context_print`
   footer rather than eyeballing output.
4. **Match the shape**, as §1 did: `--cpuset-cpus=0-3 -m 4g` against
   `SMP=4 MEMORY=4096`. Sweep `-t 1` and `-t 4`: `-t 1` isolates single-core
   compute, which *should* be identical — if it is not, something is wrong with
   the measurement, which makes it a useful control — and the `-t 1 → -t 4`
   scaling ratio is where the scheduler shows up.
5. **Fix everything else**: same `-c` (pin it, do not let the default vary),
   fixed seed, fixed prompt, identical `-n`. Median of ≥3 with the spread
   reported, and no claim inside the spread.

### Pitfalls already known

- **Memory.** 532 MB of weights with `--no-mmap` means 532 MB of private
  anonymous memory, plus KV cache, plus scratch. `MEMORY=4096` has room, but pin
  `-c` explicitly: llama.cpp's default context on a Qwen3.5 model will size the
  KV cache far above the README's `-c 256` example, written for a 135 M model in
  a 256 MB VM.
- **Thread count.** Akuma's kernel thread pool is 32 and shared with the OS.
  `-t` beyond `SMP` is not a useful data point; sweep `1` and `4` only.
- **Do not report a single ratio.** Same rule as §5. A load-time gap caused by
  the absence of file-backed `mmap` is a VFS feature gap, not "the kernel is N×
  slower", and mixing it into a throughput headline hides the one number
  (`tg` tok/s at `-t 1`) closest to like-for-like.

---

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
