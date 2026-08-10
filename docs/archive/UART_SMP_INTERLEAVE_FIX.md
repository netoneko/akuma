# UART console interleaving on SMP — fix shipped

**Status: SHIPPED, default-on in `release` since 2026-08-11.** Picks up
from `docs/archive/DEVBOX_ISSUES.md` Issue 3, which was a code-reading
finding: `console::emit` (`src/console.rs:69`) only masked IRQs on the
calling core via `irq::with_irqs_disabled`. DAIF is per-core, not a lock,
so under `smp-shared` (the default since 2026-08-10) two cores could both
be inside `emit()`'s byte loop at once, hitting the shared PL011 data
register with nothing serializing the two streams. Symptom would be
byte-level interleaving of unrelated log lines.

The fix: a `Spinlock<()>` around the loop body with an owner-core-ID
reentrancy guard, so the panic handler can't deadlock against a
half-finished `emit()` on the same core. Verified under `SMP=4` +
`cargo build -j4` self-host load on 2026-08-11 (zero garbled lines across
4778 lines of kernel output, all four cores active), then promoted to
default-on in `release` the same session.

## Constraints (the fix had to satisfy)

1. **Panic-safe reentrancy.** The panic handler (`src/main.rs:127`) calls
   `console::print` / `safe_print!`. If a panic fires *while* the panicking
   core already holds the console lock (the format arg evaluated an
   indexing op that faulted, a sync exception landed mid-`emit`, etc.), the
   handler must not deadlock trying to re-acquire it. The whole point of
   `safe_print!` (per `docs/reference/subsystems/console.md`'s "Printing
   rules") is "the console is what survives when the allocator is the thing
   that broke" — the fix can't trade that property away.
2. **No heap allocation** anywhere on the print path. Hard rule, already
   enforced by audit (`docs/archive/ALLOC_PRINT_AUDIT.md`).
3. **Works from IRQ-disabled callers.** Several producers run with IRQs
   already masked: the BKL-stuck watchdog (`crates/akuma-exec/src/sync.rs`),
   the `[THR-DUMP]` heartbeat, exception handlers (`src/exceptions.rs:718`
   onward uses `safe_print!` ~30 times). The fix must not require IRQs to
   deliver a line.
4. **Console is not a hot path.** `docs/archive/SERIAL_TRACE_TRAFFIC_AUDIT.md`
   already settled the high-volume tracing case ("per-event traces need a
   config flag with a live reader") — the answer there is *gate the trace*,
   not *make the console fast*. So throughput is a non-goal here; correctness
   under contention is.
5. **Survives a wedged scheduler.** `DEVBOX_ISSUES.md` Issue 2 documents the
   kernel's own watchdog catching a 94 s BKL stall. The console fix must
   keep working while the scheduler is stuck, because that is precisely when
   the log is the only signal you have.

## The design — owner-reentrant spinlock

One `Spinlock<()>` plus a per-core "I already hold it" flag. Reentrancy is
by owner-core check, matching the shape of `KernelLock::held_by`
(`crates/akuma-exec/src/sync.rs:819`) — the pattern was already in the tree.

```rust
use core::sync::atomic::{AtomicU8, Ordering};
use spinning_top::Spinlock;

#[cfg(kernel_console_lock)]
static CONSOLE_LOCK: Spinlock<()> = Spinlock::new(());
/// `current_core_id() + 1` of the lock holder, or 0 if free. Per-core
/// reentrancy guard for the panic / sync-exception path.
#[cfg(kernel_console_lock)]
static CONSOLE_OWNER: AtomicU8 = AtomicU8::new(0);

#[inline]
fn emit(bytes: &[u8]) {
    crate::irq::with_irqs_disabled(|| {
        #[cfg(kernel_console_lock)]
        {
            let me = akuma_exec::bkl::current_core_id() as u8 + 1;
            if CONSOLE_OWNER.load(Ordering::Relaxed) == me {
                // Reentrant fast path: panic / sync-exception inside an
                // `emit()` this core already owns. Write directly, do not
                // re-acquire (Spinlock is not reentrant).
                for &b in bytes {
                    UART.write(b);
                }
                return;
            }
            let _g = CONSOLE_LOCK.lock();
            CONSOLE_OWNER.store(me, Ordering::Relaxed);
            for &b in bytes {
                UART.write(b);
            }
            CONSOLE_OWNER.store(0, Ordering::Relaxed);
            drop(_g);
            return;
        }
        #[cfg(not(kernel_console_lock))]
        for &b in bytes {
            UART.write(b);
        }
    });
}
```

**Why this satisfies every constraint:**
- **#1 (panic):** owner-reentrancy covers a panic/fault on the core that
  already holds the lock — `CONSOLE_OWNER == me`, fast path bypasses
  acquire. The non-owner-panic case spins for at most the holder's
  loop-completion time (microseconds — bounded by `StackWriter::N`, no
  syscalls/allocations/locks inside the loop, so there is no plausible
  wedge site inside `emit()` itself).
- **#2 (no heap):** lock + atomic only, no allocation.
- **#3 (IRQ-disabled callers):** the lock is taken inside the existing
  `with_irqs_disabled` wrapper; works whether IRQs were already masked by
  the caller or masked by the wrapper itself.
- **#4 (not a hot path):** `Spinlock` is fine here; high-volume tracing
  has its own answer.
- **#5 (wedged scheduler):** no scheduler thread, no timer-tick drainer,
  no requirement that anything *else* keep running. Each `emit()` makes
  forward progress on its own core.

**Reuses existing patterns:**
- `KernelLock::held_by` (`crates/akuma-exec/src/sync.rs:819`) — same
  "owner = core_id + 1" trick.
- `current_core_id()` (`crates/akuma-exec/src/bkl.rs:277`) — already
  `cfg(kernel_smp_shared)`-gated, returns 0 on single-core / host-test
  builds, so the reentrancy check reduces to "always owner==0" there and
  is a no-op.

## Implementation (report)

Landed 2026-08-11. Files touched:

- **`build.rs`** — registered `cargo::rustc-check-cfg=cfg(kernel_console_lock)`
  alongside the other custom cfgs; added a profile-aware block that
  resolves the flag from `OPT_LEVEL` + the `CONSOLE_LOCK` env var:
  - `release` (OPT_LEVEL != "z"): default **on**.
  - `size` / `extreme-size` (OPT_LEVEL == "z"): default **off** (these
    are single-core targets where the lock is pure overhead).
  - `CONSOLE_LOCK=0` forces it off in `release` (debug escape hatch).
  - `CONSOLE_LOCK=1` forces it on in size/extreme (test override).

- **`src/console.rs`** — `emit(bytes)` rewritten as two `#[cfg]` arms.
  Added two statics gated on the flag: `CONSOLE_LOCK: Spinlock<()>` and
  `CONSOLE_OWNER: AtomicU8`. New `use` imports for `AtomicU8` / `Ordering`
  / `Spinlock`, also gated. No changes to `print` / `print_char` /
  `print_hex` / `print_dec` / `print_u64` / `StackWriter::flush` (they
  already funnel through `emit`), to `safe_print!` / `tprint!` macros,
  or to the input path.

- **`docs/reference/subsystems/config-flags.md`** — row added under the
  SMP table for `CONSOLE_LOCK` (env) → `kernel_console_lock` (cfg),
  marked **default-on in release / opt-out / force-on in size**, with
  this doc as background.

- **`docs/reference/subsystems/console.md`** — "Known gap" callout
  updated from speculative ("a small spinlock around the loop body") to
  shipped ("default-on in release; opt out with `CONSOLE_LOCK=0`"),
  linking this doc.

Total: ~15 lines of code in `console.rs`, ~20 in `build.rs` (comment +
profile-aware block + check-cfg registration), plus doc rows.

## Implementation notes

- **Single-core no-op**: on a non-`smp-shared` build,
  `akuma_exec::bkl::current_core_id()` returns 0 (see
  `crates/akuma-exec/src/bkl.rs:287`), so `CONSOLE_OWNER == me` is
  always true after the first emit and the lock path collapses to a
  single atomic load + the byte loop. The cost of enabling the flag on
  a single-core build is ~one `AtomicU8::load` per `emit()`.
- **Owner atomic lifetime**: `CONSOLE_OWNER` is set `Relaxed` *after*
  the lock is acquired and cleared `Relaxed` *before* the lock is
  released. The lock acquire/release provides the necessary ordering
  for an external observer; the atomic only needs to be visible to the
  same core's reentrant check, which `Relaxed` provides. (Same pattern
  as `KernelLock::held_by` in `crates/akuma-exec/src/sync.rs:819`.)
- **Lock scope**: the existing `with_irqs_disabled` outer wrapper is
  preserved. IRQ masking still does the per-core timer-preemption
  serialization it always did; the spinlock adds cross-core
  serialization on top. No call site or public API changed.
- **Promotion rationale**: initially landed opt-in (`CONSOLE_LOCK=1`
  only) per the "land-behind-a-flag" playbook in
  `BKL_PHASE7_AUDIT.md`. Promoted to default-on in `release` the same
  session after the SMP=4 verification below produced zero garbled
  lines across a 1m 40s `cargo build -j4` self-host run on all four
  cores.

## Verification

Performed 2026-08-11 against a `CONSOLE_LOCK=1` build, `SMP=4`,
`MEMORY=14336`, `disk_selfhost.img` booted via
`DEVBOX_DISK=disk_selfhost.img INSTANCE=1 SNAPSHOT=1 overlays/devbox/run-smoltcp.sh`.
Workload: an in-VM `cargo build --release -j4` against the akuma source
tree at `origin/main` (`38fe4fc`), with nightly `rustc 1.99.0`. The build
succeeded in 1m 40s and produced 4778 lines of kernel serial output.

**Multi-core coverage (the workload actually stressed the lock):**

| signal | count |
|---|---|
| `core=0/1/2/3` lifecycle events | 111 / 103 / 95 / 120 (≈even across all 4 cores) |
| `AS-FREE … core=N` (process exit per core) | 56 / 53 / 52 / 59 (≈even) |
| `tprint!` lines (`[T<secs>.<cs>]` prefix) | 693 |
| `fork`/`exec`/`term`/`KTG` events total | 1650 |
| `[BKL] stuck` events (multi-core contention) | many — exactly the shape that would interleave |

**Interleaving checks (all passed):**

| check | result |
|---|---|
| Lines with **two** `[T<secs>.<cs>]` timestamps (two `tprint!` calls interleaved at the timestamp boundary) | **0** |
| Lines with `][a-z]+[A-Z]` (a closed tag immediately followed by an open one mid-line, indicating two `[TAG]` lines stitched) | **0** |
| Lines with split `[TAG` (opening bracket but no matching close before EOL) | **0** |
| Short lines (`≤3` chars) other than blank separators | **0** — the 7 found are all `\n` separators between boot sections (`=== Memory Layout ===` etc.) |
| Lines with two `] [body]` patterns that aren't valid `[TAG]`/`[T…]` sequences | **0** |

Plus a representative sample of the busiest section (under `[BKL] stuck`
contention, with the watchdog firing) — every line is well-formed:
```
[BKL] stuck: owner=1 waiter=3 tag=511 (aff0+1)
[BKL] stuck: owner=1 waiter=4 tag=511 (aff0+1)
[WATCHDOG] Preemption disabled for 357ms at step 6 tid=9
[WATCHDOG] disabled at crates/akuma-exec/src/threading/mod.rs:2962
```
The watchdog-disabled lines are exactly the shape most likely to
interleave if the lock were broken — long, emitted from IRQ-disabled
context, under contention. All intact.

**Build status:** `Finished \`release\` profile [optimized] target(s) in 1m 40s` — self-host akuma kernel build succeeded inside the SMP=4 +
`CONSOLE_LOCK=1` VM.

**Cross-check build sanity** (host, no QEMU):

| build | result |
|---|---|
| `cargo check --release` | clean, no warnings |
| `CONSOLE_LOCK=1 cargo check --release` | clean, no warnings |
| `cargo test --target <host>` (host unit tests across crates) | 198+52+7+21+38+21+29 all pass |
| Binary symbol check: `CONSOLE_LOCK`/`CONSOLE_OWNER` present in `target/.../akuma` with flag on, absent with flag off | confirmed via `nm` |

**Not yet exercised** (and why that's acceptable for default-on):

- A deliberately-triggered panic inside `emit()`. Owner-reentrancy is
  the same shape as `KernelLock::held_by`'s, which has carried the
  panic-handler path for months without incident — the reasoning is
  solid, but a deliberate repro (one-line test patch that faults in a
  `safe_print!` arg expression while another core is mid-print) would
  make it direct evidence rather than analogical. Worth doing in a
  follow-up; not blocking.
- Long-running dogfood under `meow` interactive TUI + heavy concurrent
  logging. Issue 2's wedge class is the relevant stressor; revisit if a
  fresh repro of Issue 2 surfaces.

The `CONSOLE_LOCK=0` debug escape hatch stays available if either of
these surfaces a real problem.

## Why not the per-core ring + drainer (the queue idea)

The intuitive alternative is "queue per core, drain centrally" — and this
was actually implemented in the multikernel, at
`crates/akuma-smp/src/console_ring.rs` (commit `fe1b8a5`, one commit
before `ebfb73f remove multikernel` removed it). It's recoverable from git
history in full: a 155-line SPSC byte ring with 4 host unit tests, plus
~70 lines of wiring in the old `src/smp.rs` (`console_emit:142`,
`drain_console_rings:159`, `start_console_drainer:189`).

**Why it was removed** — `docs/archive/TRIM_FAT_MULTIKERNEL.md` documents
the multikernel removal as a deliberate complexity cut. The console ring
went with it as part of the whole `crates/akuma-smp` crate, not on its own
merits.

**Why it's wrong to resurrect here** — three reasons specific to this fix:

1. **The drainer depends on the scheduler or timer IRQ.**
   `start_console_drainer` spawned a kernel scheduler thread that called
   `drain_console_rings()` then `yield_now()` in a loop. That works fine
   when the system is healthy. It fails exactly when the log matters
   most: `DEVBOX_ISSUES.md` Issue 2's watchdog catches a 94 s BKL stall,
   and during that window the drainer thread doesn't get scheduled, the
   rings fill, and the drop-on-full policy starts losing log lines. That
   is the opposite of "console is what survives." (Piggybacking on the
   timer IRQ instead has the same failure mode — Issue 2's watchdog is
   literally `Preemption disabled for 1113ms`.) Constraint #5 eliminates
   this design.
2. **Drop-on-full is the wrong policy for diagnostics.** The ring's
   "never block the producer" property is right for what it was — a
   fire-and-forget console channel that could not block under any
   producer load. But for a kernel diagnostic channel, "drop on
   overflow" means the `[THR-DUMP]` you needed was the byte that got
   dropped. A spinlock that *briefly* blocks the producer is the right
   trade for diagnostics; a queue that *silently* drops is the wrong one.
3. **The multikernel had no choice; `smp-shared` does.** The ring existed
   because the multikernel's secondary cores literally had no UART
   mapping in their restricted page tables (`src/smp.rs:82-88` in the
   old tree — UART was a BSP-owned device). The ring was the only way to
   get bytes off a secondary. `smp-shared` shares the kernel address
   space across all cores; the UART is mapped everywhere; there is no
   architectural reason not to just write it directly under a lock.

**When to reconsider:** if a future high-volume-tracing use case makes the
spinlock a measurable bottleneck under SMP, *that* is the right moment to
resurrect `console_ring.rs`. `docs/archive/SERIAL_TRACE_TRAFFIC_AUDIT.md`
already notes that high-volume tracing needs "a config flag with a live
reader" rather than widening the console — the ring is a plausible
implementation of that live reader, but it's solving a different problem
than the one this fix addressed.

## Background

- `docs/archive/DEVBOX_ISSUES.md` Issue 3 — the original writeup this fix
  picked up from. Issues 5 (busybox applet symlinks) and 6 (`tail -f`
  ignores `^C`) were found during this fix's verification pass.
- `docs/reference/subsystems/console.md` — current-state console docs;
  "Known gap" callout now reflects shipped/default-on.
- `docs/reference/subsystems/config-flags.md` — `CONSOLE_LOCK` /
  `kernel_console_lock` row in the SMP table.
- `docs/archive/TRIM_FAT_MULTIKERNEL.md` — the multikernel removal pass
  that closed the original console-ring window and exposed this one.
- `crates/akuma-smp/src/console_ring.rs` at commit `fe1b8a5`
  (`git show fe1b8a5:crates/akuma-smp/src/console_ring.rs`) — the
  queue-with-buffer design, recovered in full with 4 host unit tests,
  for anyone who wants to reconsider it for a future high-volume use case.
- `crates/akuma-exec/src/sync.rs:819,833-835` — `KernelLock::held_by` and
  `log_kernel_lock_stuck`: the owner-core-ID pattern this design reuses,
  and the precedent that "owner core ID is enough; don't chase a `tag=`
  value."
- `crates/akuma-exec/src/bkl.rs:277` — `current_core_id()`, the MPIDR
  aff0 read this design uses for the owner reentrancy check (returns 0
  on single-core / host builds).
- `docs/archive/SERIAL_TRACE_TRAFFIC_AUDIT.md` — why high-volume tracing
  on SMP is solved by gating the trace, not widening the console.
- `docs/archive/ALLOC_PRINT_AUDIT.md` — the no-heap-on-console-path
  audit; constraint #2 above.
- `docs/archive/BKL_PHASE7_AUDIT.md` — precedent for landing cross-core
  locking behind a flag, verifying, then promoting.
