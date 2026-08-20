# Loopback ring conversion, and the doorbell it was missing (2026-08-20)

**Status: two fixes landed and verified.** `LoopbackAwareDevice`'s internal
queue is now a fixed-capacity ring, not a `VecDeque<Vec<u8>>` (§1-§3). That
alone did **not** close the latency gap to the NIC path — measured
afterward, loopback round trips were still ~5.4 ms at p50 against
~0.66-0.79 ms for the interrupt-driven NIC path documented in
`AKUMA_NET_ISSUES.md` (§4). Root cause: a loopback push never rang the
cross-core doorbell that document built for the NIC, so a loopback-blocked
waiter rode the old tick-bound cadence that document replaced for external
traffic. §5 wires a loopback push into the same doorbell. Measured result:
**p50 5.4 ms → 1.2 ms** (4.5x), now within ~1.6x of the NIC path instead of
~7x. p90/p99 still trail the NIC path by a smaller margin — open, not
chased further this session (§5).

## 1. What was wrong

`LoopbackAwareDevice` (`crates/akuma-net/src/smoltcp_net.rs`) intercepts
Ethernet frames addressed to 127.x.x.x in `TxToken::consume` and queues them
internally instead of sending them out `VirtIO`; `receive()` drains that
queue ahead of the wire. Until this change the queue was
`VecDeque<Vec<u8>>`, which two prior investigations had already flagged:

- `AKUMA_NET_ISSUES.md` (§ "one per-packet allocation remains"): every
  loopback frame paid a zeroing heap allocation and a copy.
- `BENCHMARK_PERFORMANCE_ATTEMPT_0.md` §6: same finding, from the Redis
  benchmarking pass — "every loopback frame costs a zeroing heap allocation,
  a second full copy of...".
- `SCHEDULING_INVESTIGATION.md` (loopback path section): flagged the queue as
  having "no capacity bound and a fresh heap allocation per loopback frame —
  a real unbounded-queue-without-backpressure smell" (not implicated in the
  hang under investigation there, but real).
- `FREEZE_INSTRUMENTATION_PLAN.md` F5: proposed capping the queue and adding
  a drop counter as a defensive measure against exactly this growth.

None of these had been acted on. The ask this session was explicit: replace
the `VecDeque` with a fixed-capacity ring, following the same shape already
proven for the virtio device buffers (`crates/akuma-net/src/virtio_rings.rs`,
`net-noalloc`'s `RxRing`/`TxRing`).

## 2. The fix

`crates/akuma-net/src/smoltcp_net.rs`, in the "Loopback-Aware Device Wrapper"
section. `LoopbackAwareDevice::loopback_queue: VecDeque<Vec<u8>>` became
`LoopbackAwareDevice::loopback: LoopbackRing`, a hand-rolled fixed-capacity
ring:

```rust
const LOOPBACK_FRAME_BUF: usize = 2048;   // matches virtio_rings::FRAME_BUF
const LOOPBACK_RING: usize = 32;          // one TCP-handshake-shaped burst

static mut LOOPBACK_BUFS: [[u8; LOOPBACK_FRAME_BUF]; LOOPBACK_RING] = ...;
static LOOPBACK_DROP_COUNT: AtomicUsize = ...;

struct LoopbackRing { lens: [u16; LOOPBACK_RING], head: usize, tail: usize, count: usize }
```

- `push(&mut self, frame: &[u8])` copies into the next free slot (still one
  copy — smoltcp hands the frame in on the stack, there is no way around
  copying it somewhere) and drops-and-counts on overflow instead of growing.
  `LOOPBACK_DROP_COUNT` / `loopback_drop_count()` is the FREEZE_INSTRUMENTATION_
  PLAN F5 ask.
- `pop(&mut self) -> Option<&'static mut [u8]>` hands back a borrowed slice
  into the static storage — no allocation, no ownership transfer.
- The buffer storage (`LOOPBACK_BUFS`, 64 KiB) is a module-level `static
  mut`, not a `LoopbackAwareDevice` field, for the same reason
  `virtio_rings.rs` keeps `RX_BUFS`/`TX_BUFS` out of the device struct:
  `NetworkState` (which owns the device) is built on the stack before being
  moved into the `NETWORK` static, and 64 KiB inline would blow that stack
  frame.

**Why holding raw `'static` slices into shared storage is sound**: every
push and pop happens under the `NETWORK` spinlock (push from
`TxToken::consume` during egress, pop from `Device::receive` during
ingress), so there is exactly one thread touching the ring at a time. A slot
`pop` hands out is only reused by a `push` after `LOOPBACK_RING` further
pushes wrap `tail` back around to it, and `RxToken::consume` — the only
reader of a popped slot — is synchronous and returns before `receive()` can
be called again. The one nested case (smoltcp generating an immediate reply,
e.g. an ICMP echo, from inside the closure the rx token's `consume` was
handed) writes to `tail`, a different slot from the `head` slot still being
read, as long as `LOOPBACK_RING >= 2`. Full argument is in the doc comment on
`LoopbackRing` in `smoltcp_net.rs`.

`FrameSource::Loopback` and `LoopbackAwareRxToken::Loopback` changed from
owning `Vec<u8>` to borrowing `&'a mut [u8]` / `&'static mut [u8]` to match —
the `Virtio` variants of both enums already worked this way, so this makes
the two variants structurally the same instead of one being an outlier.

## 3. Verification

- `cargo check`/`cargo clippy -p akuma-net --target <host>`: clean.
- `cargo build --release` (real-SMP default target) and
  `scripts/build_devbox_smoltcp.sh`: both build clean — confirms the change
  compiles under the feature set that actually exercises
  `LoopbackAwareDevice` (the default `cargo build --release` target turned
  out to boot the **rump** network stack instead, per its `[rump] BSP tap
  not available` fallback message — no `[SmolNet]` log line at all — so it
  does not exercise this code; `devbox-smoltcp` does).
- Booted `devbox-smoltcp` (`SMP=4 MEMORY=4096`), full boot-test suite passed
  (memory/FS/threading/syscall suites — this profile runs with `no-tests` so
  the smoltcp-specific self-tests, including `test_loopback_connection`, are
  compiled out; functional loopback coverage instead came from live traffic
  below).
- Live functional test: started `userspace/httpd` bound to `127.0.0.1` and
  hit it with ~600 total requests across two runs (a naive per-request `curl`
  loop and a single-process multi-request `curl --next` chain) — every
  request returned `200` with the correct 23-byte body, zero curl errors.
  600 requests against a 32-slot ring is at least 18 full wraps of `head`/
  `tail`, which is the case the "why raw `'static` slices are sound" argument
  above depends on holding up under. No panic, no `[BKL] stuck`, no test
  `FAILED` anywhere in the boot log.

## 4. It did not fix the thing everyone actually cares about: latency

The ring conversion was scoped to the two documented problems — per-packet
allocation and unbounded growth. It was never expected to fix round-trip
*latency*, but it was worth measuring rather than assuming, especially since
`AKUMA_NET_ISSUES.md`'s big win for the NIC path was entirely about latency
mechanism (missing interrupt), not allocation.

**Method**: `userspace/httpd`'s own `HTTPD_STATS=1` per-request phase timer
(the same instrumentation `AKUMA_NET_ISSUES.md` §2 built and used), read
after driving traffic from a *single* client process making N sequential
requests — a naive per-request shell loop was tried first and rejected: it
respawns `curl` every iteration, and process-spawn cost on Akuma (a fresh
ELF load) dominates the number so completely that loopback and the NIC path
became statistically indistinguishable (both ~17-19 ms/req) for reasons that
have nothing to do with either network path. Switching to one `curl`
process issuing 200 requests via `--next` (still real, separate TCP
connections and `accept()`s server-side; no client process-spawn between
them) is what separated the two paths again.

Same devbox-smoltcp boot as §3, same machine, back-to-back:

| path | client | p50 | p90 | p99 |
|---|---|---:|---:|---:|
| loopback (127.0.0.1:9090) | guest-internal, single `curl` process | **5,407 us** | 6,292 us | 6,971 us |
| NIC (host:8080 -> guest) | host, single `curl` process | **786 us** | 5,189 us | 6,626 us |

The NIC number lands right next to `AKUMA_NET_ISSUES.md`'s own controlled
measurement (657 us p50, Linux control 576 us) — good sanity check that nothing
else on this machine had regressed. Loopback is **~7x slower at p50** than a
round trip that has to leave the box, cross QEMU's virtio-net emulation, and
come back.

httpd's own steady-state phase breakdown confirms the RTT gap is entirely in
`accept` (the wait for the next connection), not the server:

```
loopback: [50 req, 18,812 us/req wall] accept=17,232us(91%) read=978us(5%) file=88us write=176us log=333us tx=23B/req
NIC:      [50 req, 16,796 us/req wall] accept=15,231us(90%) read=8us(0%)   file=75us  write=64us  log=1,411us tx=23B/req
```

(These per-window wall numbers are dominated by the same client-loop
artifact as the naive test above and should not be read as the RTT figure —
the `curl --next` table above is the clean one. They are included here only
to show `accept` is where all the time goes on both paths, which the
`--next` numbers can't show directly since httpd's own stats don't
distinguish `curl --next`'s pipelining from separate processes.)

### The likely mechanism

`AKUMA_NET_ISSUES.md`'s fix for the NIC path was registering the virtio-net
RX interrupt and using it to ring a cross-core doorbell
(`NIC_WAKE_PENDING`, `src/main.rs` `nic_irq_handler`) that ends every core's
`wfi`/`blocking_relax_net` halt immediately instead of waiting for the
periodic timer tick. That doorbell is rung from exactly one place: the
virtio-net IRQ handler.

A loopback frame never touches virtio — `is_loopback_frame` diverts it in
`TxToken::consume` before it would have reached `VirtIONetRaw::send`/
`transmit_begin` — so **no loopback push ever rings `NIC_WAKE_PENDING`**.
`netpoll_drain_step`'s own `while poll() { .. }` loop (`src/main.rs`) will
drain a loopback frame immediately *if it is already running* when the
frame lands, because a diverted frame still makes `iface.poll()` report
`PollResult::SocketStateChanged` on the sender's own pass. But getting
`netpoll_drain_step` to run promptly *at all* — and getting a thread parked
in `blocking_relax_net` waiting on the receiving socket to wake up after
it does — depends on that same doorbell everywhere else in the system. For
loopback traffic specifically, nothing ever rings it, so both sides fall
back to being woken by the timer tick. The measured p50 (5.4 ms) is
consistent with roughly one-to-two tick intervals, which is the same
tick-bound shape `AKUMA_NET_ISSUES.md` measured and fixed for the NIC path
(pre-fix: 4.9 ms average park) — evidence this is the same mechanism,
recurring on a path the interrupt fix never reached.

## 5. The fix: ring the same doorbell from `push`

Confirmed the diagnosis by acting on it. `nic_irq_handler`'s doorbell logic
(swap-and-broadcast on `NIC_WAKE_PENDING`) was extracted from the IRQ handler
into a standalone `fn ring_netpoll_doorbell()` (`src/main.rs`) that both the
IRQ handler and a new caller can invoke. The static it coalesces through was
renamed `NIC_WAKE_PENDING` -> `NETPOLL_WAKE_PENDING` — it now has two ringers
with nothing NIC-specific about either of them, and the old name would have
lied.

The new caller reaches `crates/akuma-net` (which cannot call
`ring_netpoll_doorbell` or touch the GIC directly — it is a host-testable
crate and deliberately has no hardware/IRQ dependency) through the same
seam every other kernel capability it needs already goes through:
`akuma_net::runtime::NetRuntime`, a table of plain `fn` pointers the kernel
registers once at boot (`uptime_us`, `yield_now`, `blocking_relax`, …). One
field was added:

```rust
pub wake_netpoll: fn(),
```

wired at registration (`src/main.rs`, the `akuma_net::init(NetRuntime { .. })`
call) to `ring_netpoll_doorbell`, and called from `LoopbackRing::push`
(`smoltcp_net.rs`) right after a frame is successfully queued — not on the
overflow/drop path, where there is nothing for anyone to wake up for.

This is the same seam an earlier fix in this file could have used and
didn't need to: the ring conversion (§1-§3) stayed entirely inside
`akuma-net` because allocation and queue depth are pure data-structure
concerns. Waking a parked core is not — it is a kernel capability
(`gic::broadcast_sgi`, the same one `nic_irq_handler` already used) that
`akuma-net` was never going to be given directly, which is exactly why
`NetRuntime` exists as a seam in the first place.

### Result

Same `devbox-smoltcp` boot, same machine, same method as §4 (`httpd`
`HTTPD_STATS=1`, one client `curl` process issuing 200 sequential requests
via `--next` so client process-spawn cost cannot confound the number),
run back-to-back with a fresh NIC-path measurement for a same-session
reference:

| path | p50 | p90 | p99 | mean |
|---|---:|---:|---:|---:|
| loopback, **before** this fix | 5,407 us | 6,292 us | 6,971 us | 4,481 us |
| loopback, **after** this fix | **1,211 us** | 4,151 us | 6,371 us | **2,092 us** |
| NIC path (same session, reference) | 776 us | 2,721 us | 3,775 us | 1,100 us |

p50 dropped **4.5x** and mean dropped **2.1x** — loopback went from ~7x
slower than the NIC path to ~1.6x slower. Zero curl errors, zero drops,
zero panics/`[BKL] stuck` across the boot.

**p90/p99 still trail the NIC path by a real margin** (4,151/6,371 us vs
2,721/3,775 us) — smaller than before, but not closed. Plausible causes not
chased this session: the doorbell coalesces (one broadcast per drain cycle,
same as the NIC path already accepts — see the `NETPOLL_WAKE_PENDING` doc
comment), TCP connection teardown/`TIME_WAIT` and `pending_removal` GC run
on their own timers regardless of this fix, and a `curl --next` chain is
still one client process serializing 200 real TCP connections so some tail
variance is inherent to the harness, not the kernel. Worth a closer look if
loopback tail latency matters to a real workload; not pursued further here.

## Background

- [`AKUMA_NET_ISSUES.md`](AKUMA_NET_ISSUES.md) — the NIC-path investigation
  this one keeps comparing against; §6 (the interrupt fix) and §9 (the
  doorbell re-arm race) are the two sections most relevant to §4 above.
- [`BENCHMARK_PERFORMANCE_ATTEMPT_0.md`](BENCHMARK_PERFORMANCE_ATTEMPT_0.md)
  §6 — first flagged the loopback per-packet allocation.
- [`SCHEDULING_INVESTIGATION.md`](SCHEDULING_INVESTIGATION.md) — flagged the
  unbounded `VecDeque` growth (not implicated in that investigation's hang).
- [`FREEZE_INSTRUMENTATION_PLAN.md`](FREEZE_INSTRUMENTATION_PLAN.md) F5 — the
  original ask for a capacity cap and a drop counter.
- [`LOOPBACK_TIMEOUT_FIX_PLAN.md`](LOOPBACK_TIMEOUT_FIX_PLAN.md),
  [`LOOPBACK_ARP_RATE_LIMIT_BUG.md`](LOOPBACK_ARP_RATE_LIMIT_BUG.md),
  [`DHCP_LOOPBACK_TEST_FIX.md`](DHCP_LOOPBACK_TEST_FIX.md) — earlier
  loopback correctness bugs (all about the two-`poll()`-calls shape loopback
  traffic requires), unrelated to this session's allocation/latency work but
  the reason `LoopbackAwareDevice` looks the way it does.
