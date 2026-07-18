# Rump sysproxy latency — Phase 2 + 3 fix (scheduling, poll cadence, Nagle)

> **Status: LIVE (2026-07-18).** Phase 2a + 2b + 3a + 3b applied, measured, verified.
> See `docs/archive/RUMP_LATENCY_SLEEP_FIX.md` for the earlier **disproven**
> approach (lowering `hz`, `cv_broadcast`→`cv_signal`) — do NOT revisit.

## Symptom

Devbox SSH sessions (sshd on rump stack) were slow: `uname -a` over SSH took
~3.4 s, `echo hello` ~4.0 s. Interactive `busybox` input and `meow` screen
refresh felt laggy.

## Root cause

Two independent scheduling bottlenecks in the rump sysproxy path:

### 1. Network thread boost targeted the wrong thread (Phase 2a)

`start_default_stack` (`src/rump_proxy.rs`) spawns rump_server and immediately
discards the returned TID:

```rust
Ok((_tid, _chan, pid)) => pid,   // ← _tid thrown away
```

The scheduler's `NETWORK_THREAD_RATIO` boost (pre-empt to the network thread
every N ticks) was instead claimed by the proxy handshake kthread in
`attach_server` — a thread that parks in `loop { yield_now(); }` after the
one-time `Client::connect` handshake and does **zero** per-call sysproxy work.
Boosting a parked thread is a no-op.

The actual per-call work flows through rump_server's main OS thread (the fiber
scheduler). Without the boost, rump_server waited 4–9 scheduler quanta (10 ms
each) for a timeslice under load. Per-call sysproxy blocking was 84–104 ms.

### 2. Blocking-poll cadence too coarse for rump sockets (Phase 2b)

`sys_ppoll` / `sys_epoll_pwait` / `sys_pselect6` used a 10 ms re-poll floor
(`POLL_BLOCK_INTERVAL_US = 10_000`). When a process polls a rump socket fd,
10 ms of idle wakeups accumulate before the fd is re-checked. With many proxied
recvfrom/sendto calls per SSH session, this multiplied into hundreds of
milliseconds.

## Fix

### Phase 2a — register rump_server's TID (not the kthread's)

In `start_default_stack`, capture the TID returned by
`spawn_process_with_channel` and register it:

```rust
let (server_tid, pid) = match process::spawn_process_with_channel(...) {
    Ok((tid, _chan, pid)) => (tid, pid),
    ...
};
threading::set_network_thread_id(server_tid);
```

Removed the stale `set_network_thread_id(current_thread_id())` from the proxy
kthread in `attach_server` (it was behind `#[cfg(feature = "rump-default")]`
but was a no-op because the kthread parks after handshake).

Also: `run_async_main` in `src/main.rs:1161` already gates its own
`set_network_thread_id` behind `#[cfg(not(feature = "rump-default"))]` so the
two registrations don't conflict.

### Phase 2b — rump-aware 1 ms poll cadence

Added `RUMP_BLOCKING_POLL_INTERVAL_US = 1_000` in `src/syscall/poll.rs` and
`effective_poll_interval_us(has_rump_fd: bool)` which returns the shorter
interval when any polled fd is a rump socket. Helper functions:
`fd_is_rump_socket`, `any_fd_is_rump_socket`, `fd_set_contains_rump_socket`.

Three call sites updated:
- `sys_epoll_pwait` (poll.rs:743)
- `sys_pselect6` (poll.rs:847)
- `sys_ppoll` (poll.rs:932)

All gate on `#[cfg(feature = "rump")]` with `#[cfg(not(feature = "rump"))]`
stubs returning `false`.

## Measurement (2026-07-18, devbox, single kernel, QEMU virt, 4 GB)

| Metric | Baseline | Phase 2b only | Phase 2a + 2b | Phase 2a+2b+3a |
|--------|----------|---------------|----------------|----------------|
| `uname -a` over SSH (median) | ~3.4 s | ~2.7 s | 1.96 s (−42%) | **1.85 s** (−46%) |
| `echo hello` over SSH (median) | ~4.0 s | ~2.7 s | 1.97 s (−51%) | **1.81 s** (−55%) |

Per-call sysproxy trace (`RUMP_SP_TRACE=true`) before vs after:

| | Before (Phase 2b only) | After (Phase 2a + 2b) |
|---|---|---|
| `blk=` on sendto/recvfrom | 84–104 ms / 4–5 iterations | mostly **0 µs / 0 iterations** |
| Total per-call time | 84–104 ms | 50–110 ms (fiber round-trip) |

The `blk=0us/0` on most calls confirms rump_server now gets scheduled promptly
via the boost — the response arrives before the caller's deadline-based sleep
kicks in.

## Regression guards

`src/rump_tests.rs` — 5 tests, run at boot under `#[cfg(feature = "rump")]`:

- **T1**: `run_async_main`'s `set_network_thread_id` is gated by
  `#[cfg(not(feature = "rump-default"))]` (so it doesn't conflict with
  `start_default_stack`'s registration).
- **T2**: `start_default_stack` captures the TID from
  `spawn_process_with_channel` and calls `set_network_thread_id(server_tid)`.
  Also verifies the proxy kthread does NOT register itself.
- **T3/T4/T5**: `effective_poll_interval_us` returns correct values for
  non-rump (10 ms), rump (1 ms), and the ratio is meaningfully shorter.

Enabled on devbox via the `rump-tests` Cargo feature (allows rump_tests to
compile alongside `no-tests` without pulling in the full smoltcp-coupled test
suite).

## Phase 3a — tap fd was missing the tightened rump poll cadence

The original plan for Phase 3a was "add a readiness waker in `rumpcomp_tap.c`
so rump_server's fiber scheduler gets woken immediately on tap RX." That
premise doesn't hold: `/dev/net/tap0` (NIC1, `crates/akuma-net/src/rump_tap.rs`)
is **pure register-polled virtio, no RX IRQ** — nothing in the kernel
discovers a frame's arrival except a caller actively checking
`akuma_net::rump_tap::has_frame()`. There is no independent event to push a
wake from; a "waker" with nothing to fire it is a no-op. Building a real
push-wake would mean adding NIC1 GIC interrupt handling from scratch — out of
scope here (tracked as Phase 3a2 below, if ever needed).

What *was* real: `rumpcomp_tap.c`'s `rcvthread` already blocks on the tap fd
via `rumpuser_akuma_wait_fd` (fiber backend: `fiber.rs`'s `schedule()` idle
path calls the kernel's `poll()`/`sys_ppoll` on it; pthread backend: a direct
host `poll()`). That `sys_ppoll` blocking loop re-checks readiness on a fixed
cadence — 1 ms for "rump fds", 10 ms otherwise (`RUMP_BLOCKING_POLL_INTERVAL_US`
vs `BLOCKING_POLL_INTERVAL_US`, `src/syscall/poll.rs`). The classifier that
picks between them, `fd_is_rump_socket` (now `fd_wants_rump_poll_interval`),
only matched `FileDescriptor::RumpSocket` — **not** `FileDescriptor::Tap`.
So the RX kthread's per-frame wait — the highest-frequency rump-fd wait in the
whole system — was silently paying the 10 ms floor instead of the 1 ms one
Phase 2b introduced, on every SSH round-trip.

### Fix

`src/syscall/poll.rs`: renamed `fd_is_rump_socket` →
`fd_wants_rump_poll_interval` (and its `any_fd_*`/`fd_set_*` callers to
match, all private to this file) and widened its match to also accept
`FileDescriptor::Tap { .. }`. Three call sites already routed through it
(`sys_epoll_pwait`, `sys_pselect6`, `sys_ppoll`), so no new call sites were
needed — this was a pure classification fix.

## Phase 3b — Nagle + no `TCP_NODELAY` was strangling multi-write bursts

The remaining symptom after 3a: `busybox` per-keystroke echo and `meow`'s TUI
(3-pane layout, redrawn via a per-character cursor-position + glyph write —
confirmed by capturing its raw output: `\x1b[25;28He\x1b[25;29Hl…`) were still
"incredibly slow", and had never been slow under the smoltcp built-in stack.
That split (fine under smoltcp, bad under rump) plus "many small writes worse
than one big write of the same content" is the signature of Nagle's algorithm
interacting with delayed ACKs, not a scheduling problem.

Checked whether any box program could turn Nagle off: `libakuma::net` (what
`/bin/sshd` and `meow` are built against) has **no `set_nodelay` at all**, and
`proxy_setsockopt` (`src/rump_proxy.rs`) was a hardcoded no-op returning
success without forwarding anything to the real rump socket (originally added
so curl's best-effort `TCP_NODELAY`/keepalive calls wouldn't abort it — see
the function's existing doc comment). So every rump TCP socket ran with Nagle
on, permanently, with no way for a box program to opt out even if it tried.

### Measurement (interactive-latency harness, PTY over real SSH, devbox)

| Test | Before 3b | After 3b |
|---|---|---|
| Single keystroke echo (busybox, serialized round-trips) | ~320–370 ms median | ~320 ms median (unchanged — see below) |
| 50 separate `printf` writes in one command | 1427 ms | **458 ms** (−68%) |
| 200 separate `printf` writes in one command | 2070 ms | **518 ms** (−75%, barely above the 50-write case) |
| Same 50-line content as ONE write (`seq 1 50`) | 369.8 ms (already fine) | 284.5 ms |

Per-keystroke echo latency is unaffected by 3b because that test is fully
serialized (wait for the echo before sending the next key) — no unacked
backlog ever exists for Nagle to hold back, so its cost is the underlying
sysproxy/fiber round-trip floor (Phase 2's own "50–110 ms fiber round-trip",
compounded across the ≥2 rump-proxied hops in a keystroke→echo cycle), not
Nagle. The 50/200-write bursts *do* build up a backlog of small unacked
segments — that's exactly what Nagle+delayed-ACK penalizes, and exactly what
3b fixes. `meow`'s redraw is a burst-of-small-writes workload, so it benefits
the same way: post-fix, live per-keystroke TUI redraw in `meow` itself
measured ~140 ms median (captured via the same PTY harness, typing into a
running `meow` session).

### Fix

`src/rump_proxy.rs`: added `set_rump_sock_nodelay(proxy, rump_fd)` — forwards
a real NetBSD `setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, 1)` (syscall 105, via
the same `Op::Setsockopt` mapping that already existed but was unused; the
option value is served through `cin_override` the same way `connect`/`bind`
serve their translated `sockaddr_in`, so no real box pointer is needed).
Applied unconditionally — not opt-in — on every accepted rump TCP socket
(`proxy_accept`) and every outbound one (`proxy_connect`), since no box
program has a way to request it itself. `IPPROTO_TCP`/`TCP_NODELAY` are
numerically identical on Linux and NetBSD, so no translation table was
needed. `proxy_setsockopt` itself is untouched (still a no-op for whatever a
box program explicitly requests) — this is a proxy-internal default, not a
change to what setsockopt reports back to callers.

Tradeoff: Nagle exists to coalesce small writes for bulk-transfer efficiency;
disabling it unconditionally trades a small amount of packet-count efficiency
on large transfers (git clone, curl downloads) for interactive latency. Given
every fix in this doc already prioritizes interactive latency over bulk
throughput, and this is exactly what every SSH client/server and browser
defaults to, that tradeoff is the right one here.

## Also cleaned up: debug trace flags left on

`SYSCALL_DEBUG_NET_ENABLED` and `RUMP_SP_TRACE` (`src/config.rs`) had been
flipped to `true` while debugging Phase 2a/2b and never reverted — every
`ppoll`/`epoll_pwait`/rump-proxy call was logging a trace line, which was
flooding the serial console (10k+ lines for a single boot + a few SSH round
trips) and made the actual measurement logs hard to read. Both reverted to
their documented default (`false`).

## Future work (Phase 3, not started)

- **Phase 3a2**: A genuine push wake for tap RX would require NIC1 GIC
  interrupt handling (none exists today — see Phase 3a above); only worth it
  if 1 ms is still measurably too coarse after 3a.
- **Phase 3c**: Port `rumpcomp_tap.c` (241 lines, C-ABI to NetBSD
  `librumpnet_virtif`) to Rust for parity and maintainability.
- **Phase 3d**: Per-socket wakers (conditional wake instead of broadcast) to
  reduce scheduler contention under many-fd workloads.
- **Phase 3e**: The ~300 ms single-round-trip floor (Phase 2's residual
  "fiber round-trip" cost) is still unaddressed — it's the reason per-keystroke
  echo didn't improve in 3b. Worth profiling where inside a single sysproxy
  round trip that time actually goes, now that 3a/3b have removed the two
  cheaper-to-fix multipliers on top of it.
