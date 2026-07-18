# Rump sysproxy latency — Phase 2 fix (network thread + poll cadence)

> **Status: LIVE (2026-07-18).** Phase 2a + 2b applied, measured, verified.
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

| Metric | Baseline | Phase 2b only | Phase 2a (fixed) + 2b |
|--------|----------|---------------|----------------------|
| `uname -a` over SSH (median) | ~3.4 s | ~2.7 s | **1.96 s** (−42%) |
| `echo hello` over SSH (median) | ~4.0 s | ~2.7 s | **1.97 s** (−51%) |

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

## Future work (Phase 3, not yet started)

- **Phase 3a**: Add a readiness waker in `rumpcomp_tap.c` so rump_server's
  fiber scheduler gets woken immediately on tap RX, instead of polling.
- **Phase 3b**: Port `rumpcomp_tap.c` (241 lines, C-ABI to NetBSD
  `librumpnet_virtif`) to Rust for parity and maintainability.
- **Phase 3c**: Per-socket wakers (conditional wake instead of broadcast) to
  reduce scheduler contention under many-fd workloads.
