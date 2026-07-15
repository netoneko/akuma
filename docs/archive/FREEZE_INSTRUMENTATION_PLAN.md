# Network/Kernel Freeze: Instrumentation Plan (Layers 1–3)

## Context

The Akuma kernel freezes randomly after some uptime. `docs/NETWORKING_DEADLOCK_INVESTIGATION.md` documents one resolved freeze class (priority inversion between preemptible userspace and preemption-disabled async tasks holding VFS/Block locks); the remaining freezes are not yet localized. Existing instrumentation is limited: the preemption watchdog (`crates/akuma-exec/src/threading/mod.rs:605`) warns at 100ms, and `RawRwSpinlock` logs at 10M-spin (`crates/akuma-exec/src/sync.rs:113`). Neither identifies *who* is holding what, *for how long*, or what the network stack was doing at the time. The "happens after a while" signature points at accumulating state (socket table exhaustion, loopback queue growth, rare lock-order race, smoltcp panic from concurrent state mutation) — none of which we can currently see.

Goal: when the next freeze happens, a single shell command (or, if the shell is dead, a known-address QEMU monitor read) tells us which class of freeze it is.

## Freeze theories most likely given "random, after uptime"

Most likely given the signature:
- **F5** Loopback queue unbounded growth (`smoltcp_net.rs:~256-402`)
- **F6** Socket table exhaustion via TIME_WAIT accumulation (`MAX_SOCKETS=256`, `SOCKET_GC_TIMEOUT_US=30s`, smoltcp_net.rs:36)
- **F7** smoltcp internal state corruption → panic inside `iface.poll()` while holding `NETWORK` (per the underflow noted in the investigation doc §5)
- **F1/F2/F3** AB-BA or priority-inversion race that requires a specific interleaving (rare → late firing)
- **F4** `wait_until` livelock at high RX rate (`socket.rs:337`, 64-iter no-yield)

Less likely but still need coverage:
- **F9** IRQ-disabled-too-long (would also be silent and "after some time" — must rule in/out)

## Critical files

| File | Why |
|------|-----|
| `crates/akuma-net/src/smoltcp_net.rs` | `NETWORK` lock site (line 489, 593), poll loop, loopback queue, GC, `TX_DROP_COUNT` |
| `crates/akuma-net/src/socket.rs` | `wait_until` livelock site (line 329), `SOCKET_TABLE` |
| `crates/akuma-exec/src/sync.rs` | `RawRwSpinlock` stuck-detect (line 113) — extend with holder TID |
| `src/timer.rs` | 10ms tick, watchdog dispatch (line 48), SGI trigger |
| `crates/akuma-exec/src/threading/mod.rs` | Preemption watchdog (line 605) |
| `src/shell/commands/builtin.rs` | `kthreads` precedent; add `netstat` / `lockstat` |

## Layer 1 — Lock telemetry on the two hot locks

Scope kept narrow: instrument **only** `NETWORK` (in `smoltcp_net.rs`) and `SOCKET_TABLE` (in `socket.rs`). These are the locks repeatedly named in the AB-BA mitigation comments and the ones every networking path traverses. Wider coverage can come later.

**Approach:** introduce a thin `TracedSpinlock<T>` newtype in a new module (e.g. `crates/akuma-exec/src/traced_lock.rs`) wrapping `spinning_top::Spinlock<T>`. On lock acquire, store `(holder_tid: AtomicU32, acquired_us: AtomicU64, lock_id: u8)` into a small `static LOCK_STATE: [LockEntry; N]` table indexed by `lock_id`. On unlock, compute hold duration and bucket into atomic histogram counters (`<10µs, <100µs, <1ms, <10ms, >10ms`).

**Stuck-spin dump enhancement:** when an internal spin loop in the traced lock exceeds ~1M iterations, log the *current* holder TID and how long they've held it, not just the raw state word. This single change converts F1/F2/F3 from "kernel hung, no clue" into "thread 7 held NETWORK for 4200ms while thread 3 spun."

**Cost:** two atomic stores per acquire, two per release, one histogram bump per release. Negligible.

Reuse: the existing `safe_print!` macro (already used by `log_write_lock_stuck` in `sync.rs:137`) for output — no heap.

## Layer 2 — IRQ-path watchdog (rule in/out F9)

Goal: prove whether the freezes are "IRQs still firing, threads spinning" vs. "IRQs masked, system dead silent."

**Changes in `src/timer.rs:48` (timer IRQ handler):**
1. `LAST_TIMER_TICK_US: AtomicU64` — store `uptime_us()` on every tick.
2. At the top of the tick handler, compute `gap = now - prev`. If `gap > 50_000`µs (5× expected), emit `[WATCHDOG] timer gap: Xms` via `safe_print!`. This catches any path that held DAIF masked for too long.
3. Track per-IRQ entry/exit timestamps; if any handler took >1ms, log it. (Hook the generic IRQ dispatcher, not just timer.)
4. `NESTED_IRQ_DEPTH: AtomicU32` incremented on IRQ entry, decremented on exit; log if it ever exceeds 1.

If timer gaps are observed during a freeze, the answer is F9 / IRQ leak and we go hunting `IrqGuard` early-return paths. If gaps are *not* observed, F9 is ruled out and Layer 1/3 will identify the actual class.

## Layer 3 — Network-path counters (catches F4–F8)

**Add to `smoltcp_net.rs` globals (all `AtomicUsize`/`AtomicU64`, lock-free reads):**
- `LOOPBACK_QUEUE_DEPTH` — written on every push/pop of `loopback_queue`. **Also**: cap the queue at e.g. 512 frames; on overflow drop with `[NET] loopback queue overflow` and bump `LOOPBACK_DROP_COUNT`. (This converts F5 from "freeze" into "logged drop event" — but during this scope we only *observe*; the cap can be done as part of this work since it's trivially safe.)
- `RX_DROP_COUNT`, `RX_RING_FULL_COUNT`, `TX_RING_FULL_COUNT` — mirror the existing `TX_DROP_COUNT`.
- `POLL_DURATION_US_MAX`, `POLL_DURATION_US_LAST` — measured around `iface.poll()` inside the `NETWORK` lock (line 492). If max is multi-millisecond, smoltcp is doing something pathological inside the lock and Layer 1 will see the corresponding hold-time.
- `SOCKET_TABLE_OCCUPANCY`, `PENDING_REMOVAL_COUNT` — exposed for F6 detection.

**Add to `socket.rs:wait_until` (line 329):**
- A counter incremented on each iteration where `condition()` returned false; reset on success. If a single call exceeds 256 iterations without yielding, log `[NET] wait_until starvation tid=X iters=N` and force a yield. This both reports and partially mitigates F4.

## Minimal shell surface (required to read Layers 1–3)

We can't ship counters without a way to read them. Add to `src/shell/commands/builtin.rs`:
- **`netstat`** — print Layer 3 counters: poll count, tx/rx drops, ring fullness, loopback queue depth, socket table occupancy, pending_removal, last/max poll duration.
- **`lockstat`** — for NETWORK and SOCKET_TABLE: current holder TID (or `-` if free), age of current hold (if held), and the hold-time histogram buckets.

Both commands should fit in ~50 lines each and use the same printing style as `kthreads`.

## Out of scope (deferred)

- Black-box snapshot ring buffer (useful but not on critical path if Layers 1–3 already point at the cause).
- Strategy B from the investigation doc (smoltcp actor migration).
- Changing `wait_until`'s policy beyond the starvation log/forced-yield above.
- Wrapping the global `RwSpinlock` / every `spinning_top::Spinlock` — Layer 1 stays narrow to NETWORK + SOCKET_TABLE.

## Verification

End-to-end checks to run after implementation:

1. **Boot + idle** — `netstat` and `lockstat` should produce sensible numbers (zeros / no holder). Confirms wiring works without breaking anything.
2. **Synthetic loopback flood** — userspace test that opens a UDP/TCP loopback socket and blasts packets in a tight loop. Expect `LOOPBACK_QUEUE_DEPTH` to climb and (after the cap) `LOOPBACK_DROP_COUNT` to increment. Validates Layer 3.
3. **Synthetic AB-BA test** — kernel-only test that deliberately mis-orders NETWORK then SOCKET_TABLE on two threads; expect the traced-lock stuck-spin dump to print holder TIDs within seconds. Validates Layer 1.
4. **IRQ-mask test** — kernel-only test that masks IRQs via `IrqGuard` and busy-waits 100ms; expect `[WATCHDOG] timer gap` log. Validates Layer 2.
5. **Long-run reproducer** — leave QEMU running with the workload that has been freezing. When it next freezes:
   - If shell responds: run `netstat` + `lockstat` + `kthreads` and capture output.
   - If shell is dead: use QEMU monitor `xp /16xg <address>` to read the static counter table directly (note the symbols' addresses in the build output). Even total kernel hangs are now observable.
6. **Confirm no perf regression** — run an existing networking benchmark (e.g. HTTP throughput in current scratch tests) before and after; histogram instrumentation should add <1% overhead.

## Implementation order

1. `TracedSpinlock<T>` newtype + Layer 1 wiring on NETWORK and SOCKET_TABLE.
2. Layer 3 counters in `smoltcp_net.rs` + `wait_until` starvation counter in `socket.rs`.
3. Layer 2 timer-gap watchdog in `src/timer.rs`.
4. `netstat` + `lockstat` shell commands.
5. Verification tests.
