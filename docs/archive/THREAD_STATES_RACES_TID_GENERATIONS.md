# THREAD_STATES check-then-store races and tid generations (2026-08-06)

Investigation record for the session that picked up the two survivors of the
clone-tidflags fix ([`CLONE_TIDFLAGS_THREAD_LIST_LOCK.md`](CLONE_TIDFLAGS_THREAD_LIST_LOCK.md)):
the `AS MISMATCH: L0 BASE DIFFERS` fault (a thread executing in a third party's
page tables, runbook §2f) and the `[BKL] stuck` storm that wedged a `-j4` build
with live-but-idle rustc processes. Maintained summary:
[`../runbooks/debug-thread-spawn-segv.md`](../runbooks/debug-thread-spawn-segv.md) §2g.

## How the mechanism was found

The §2f evidence was one fault in one run: tid 28's live `TTBR0_EL1` named
neither its own `Process` nor its parent — a third address space entirely
(`0xF7:0x88F1D000` against `2/4:0x88B1D000`). Static analysis of the switch
path first, because that is where TTBR0 changes hands:

- `sgi_scheduler_handler_with_sp` **saves the live TTBR0 into the outgoing
  context and restores `ctx.ttbr0` for the incoming one** — sound on its own,
  and it means a foreign live TTBR0 can only come from a corrupted *saved
  context*.
- All TTBR0 writers were enumerated (`msr ttbr0_el1` appears in exactly three
  places: the switch path, `activate()`, `deactivate()`); no path borrows a
  foreign address space temporarily. So the corruption had to enter the saved
  context while the thread was off-CPU.
- That reframes the question as: *who can make a slot schedulable when its
  context does not belong to a runnable thread?* Grepping
  `THREAD_STATES[...].store` found the answers.

## The bug family: check-then-store on THREAD_STATES

All in `crates/akuma-exec/src/threading/mod.rs`. Four instances, one shape —
a load of the state followed by a separate store, with no lock held:

### 1. `ThreadWaker::wake` — the corruption vector

```rust
// BEFORE
if THREAD_STATES[tid].load(SeqCst) == WAITING {
    WAKE_TIMES[tid].store(0, SeqCst);
    THREAD_STATES[tid].store(READY, SeqCst);
    ...
}
```

Every futex/pipe/eventfd/msgqueue/timer wake funnels through this, and it runs
preemptible with no lock. A waker switched out between the load and the store
can resume **milliseconds** later. In that window the target can wake by
timeout, run, exit, have its slot reclaimed and re-claimed by a new
`clone_thread` — slot churn under `-j4` is constant (rustc's rayon pool runs
the 256-slot table at its ceiling; see "Amplifier" in the runbook §3c). The
stale `READY` store then lands on an INITIALIZING slot whose
`THREAD_CONTEXTS` entry is still the previous occupant's. A peer scheduler
picks it and restores the previous occupant's:

- **`ttbr0`** — a dead thread's address space, freed and possibly recycled: a
  *third party's* L0, which is precisely the §2f signature; and
- **kernel stack pointer** — possibly a stack still in use elsewhere: a
  double-run, which is the ON_CPU-gate corruption family
  ([`SMP_SHARED_ONCPU_GATE.md`](SMP_SHARED_ONCPU_GATE.md)) re-entered through
  a different door, and the shape that corrupts BKL bookkeeping into
  `[BKL] stuck` storms.

The same window held a second, independent bug: `WAKE_TIMES[tid].store(0)`
after the check. A stale waker's clear can erase a **fresh** deadline the
slot's new occupant just published (`mark_thread_waiting`), leaving it parked
forever with no timeout — the "live-but-idle rustc" wedge shape.

### 2-4. The TERMINATED-overwrite races

`mark_thread_terminated` is called **cross-thread with no lock**
(`kill_thread_group` killing siblings). Anywhere that did
"load state; if != TERMINATED, store X" could overwrite a TERMINATED that
landed between the two — resurrecting a killed thread whose process teardown
(address space, fds, lazy regions) was already in progress:

- `commit_switch` and the net-boost path (re-READY of the outgoing thread);
- `publish_waiting_and_take_pending_wake` and both park-loop resume arms in
  `schedule_blocking` (one stored RUNNING **unconditionally**);
- `mark_thread_ready` — the spawn publish — could resurrect a clone child
  killed by a group exit between context setup and publish.

## The fixes

1. `ThreadWaker::wake`: `compare_exchange(WAITING, READY)` — a wake can make
   no other transition. The `WAKE_TIMES` clear is **removed** (every sleep
   entry rewrites the deadline; a leftover value on a non-WAITING thread has
   no reader).
2. The scheduler wake-pass: same CAS; its `WAKE_TIMES` clear only follows a
   transition it owns (safe there — the woken thread cannot re-park while this
   core holds POOL).
3. `commit_switch` / net-boost / park resumes / `mark_thread_ready`:
   `fetch_update` that refuses TERMINATED (and WAITING where applicable).

## Tid generations (`WakeHandle`) — closing the class, not the instances

The deeper asymmetry, called out by the user: **pids are generational, tids are
not.** `allocate_pid` is a monotonic `fetch_add` — a stale pid *misses* in the
process table. A tid is an index into a recycled array — a stale tid is
indistinguishable from a live one, and every per-thread structure (states,
wakers, signals, futex queues) is keyed by the ambiguous kind. The CAS fixes
make stale-tid wakes non-corrupting but still *wrong* (a spurious wake of the
slot's new occupant when it happens to be WAITING; a phantom sticky-flag wake).

Design (same file):

- `SLOT_GEN[tid]: AtomicU64`, bumped once per slot lifetime in
  `scrub_thread_slot` — every claim path runs the scrub under the winning
  FREE→INITIALIZING CAS, so there is exactly one bumper per rebirth. Slots
  that never recycle (boot/idle/system threads) stay at generation 0, which is
  correct: their handles always validate.
- `WakeHandle` = `(generation << 16) | tid` (`MAX_THREADS` ≤ 2^16 is a
  compile-time assert; 48 generation bits outlive any uptime).
- `current_wake_handle()` / `wake_handle_for_thread(tid)` mint handles;
  `wake_by_handle` refuses a stale generation **before any side effect** —
  including the sticky `WOKEN_STATES` store.
- The `core::task::Waker` vtable packs the handle into the raw-waker data
  pointer, so `Waker`-storing registries (terminal input waker, ssh session
  waker) became incarnation-bound with no changes.

**The rule that matters:** wait queues store handles **minted by the waiter at
enqueue time**. Minting at wake time (`wake_handle_for_thread` on a tid stored
long ago) launders a stale tid into a fresh-looking handle — the API docs on
`get_waker_for_thread` now say exactly this.

Converted queues (bare `usize` → `WakeHandle`, keyed by tid where scans need
it): futex `FUTEX_WAITERS` (`src/syscall/sync.rs`, including requeue and the
diagnostic dumps), pipe pollers, msgqueue send/recv pollers, eventfd pollers
(`src/syscall/{pipe,msgqueue,eventfd}.rs`), `VFORK_WAITERS`
(`src/syscall/proc.rs`), `ProcessChannel` pollers
(`crates/akuma-exec/src/process/channel.rs`).

Left on bare tids deliberately: `pend_signal_for_thread`,
`request_thread_kill`, `kill_thread_group` — their callers resolve tids from
live process state at call time, and their per-thread array stores are a
staleness surface the wake layer cannot fix. The mechanism
(`thread_generation`, `WakeHandle::is_current`) is available if they ever
need it.

## New instrumentation

**TTBR0 tripwires** (always on): `EXPECTED_L0[tid]` tracks the L0 base each
thread should run under (writers: `update_thread_context` for child inits —
NOT for the current thread, where the live truth belongs to `activate()`;
`activate`/`deactivate` under a new IrqGuard so install+note are atomic; scrub
clears to 0 = skip). The switch path checks both directions:

- `[TTBR SAVE-MISMATCH]` — the outgoing thread was RUNNING under someone
  else's L0, and the save just made it permanent in its context;
- `[TTBR LOAD-MISMATCH]` — the incoming thread's saved context was corrupted
  while it was off-CPU.

ASIDs are masked out (a cloned thread legitimately runs under the parent's L0
with a different ASID — the §2f "read the flag carefully" lesson). Zero hits
across full SMP=4 boot suites. A future `-j4` EL0 `AS MISMATCH` fault with
zero tripwire lines before it would mean the corruption enters outside the
switch/context machinery — as valuable as a hit.

**`[SGI-S STACK]` de-noised**: it fired ~20k times per SMP=4 boot for
tids 1-3 — per-core idle threads are seeded at bringup on their per-core
*boot* stacks, not the pool stacks registered for their slots, so every switch
into an idle thread tripped it (present on known-good boots; pure noise). Now
gated by `IS_IDLE_THREAD`; a remaining line for a non-idle tid is a real
finding.

## Measurements

Boot probes: SMP=4, `--features smp-shared` (full boot suite), 165 s window,
`[BKL] stuck` counted over the window and again 15 s later (rate at end),
then an ssh handshake:

| kernel | suite | BKL total ×3 runs | rate at end | ssh |
|---|---|---|---|---|
| HEAD (1b21e63) | 258 PASSED, 0 FAILED | 70 / 72 / 74 | 0 | ok |
| + CAS fixes + tripwires | 259 PASSED, 0 FAILED | 91 / 75 / 72 | 0 | ok |
| + tid generations | 259 PASSED, 0 FAILED | 68 | 0 | ok |

Reading: the boot-time storm (onset consistently at the "NEON registers across
preemptive scheduling" test) is **pre-existing and statistically identical**
across all three kernels — a transient burst that recovers. One pre-A/B boot
of the fixed kernel showed the rarer *persistent* form (6.5k events, owner
migrated 1→4, ssh dead) — the same persistent form is in the historical record
for gated kernels, so it reads as variance, not regression; but the storm
itself remains an open, separate issue. **A/B it as a rate, never a boolean.**

Host: 490+ tests pass, including the new
`threading::state_transition_guard_tests` (5 tests — waker refuses
INITIALIZING/TERMINATED/FREE/RUNNING; deadline untouched; publish and resume
refuse TERMINATED; and the decisive
`a_stale_handle_is_refused_even_against_a_waiting_new_occupant`, the case CAS
alone cannot defend). Boot suite: `wake_transition_guards` in
`src/process_tests.rs` (refusal semantics only — flipping a contextless test
slot READY in a live kernel would invite the scheduler to run it).

## Status at time of writing

The `-j4` self-host verification (cold build, cloned disk, SMP=4) ran twice
against the full change set:

- **Run 1** (pre-discriminator kernel): reproduced the §2f-class fault at the
  OLD handoff's exact PC (`ELR=0x116748ac` = rustc+0x16748ac, FAR as ASCII
  `"generic-array.rs"`, fresh clone dead at 0.01 s, third-party L0) with
  **zero TTBR tripwires and zero storm before it** — proving the corruption
  enters outside the switch/context machinery. The fault dump was then
  extended with `expected_l0` / `switch_ins` / `slot_gen` discriminators.
- **Run 2** (discriminator kernel): **35+ minutes with ZERO spawn faults,
  zero AS mismatches, zero tripwires, zero `[BKL] stuck`** — the longest
  clean stretch in the record (historic rate: one fault per 2-4 min). Not
  proof (run 1 faulted once in ~10 min on the same fixes), but the class-1
  criteria were all green when the run ended — ended by the OTHER class:
  the ld-musl instruction aborts killed two rustcs and a cargo child at
  T≈33-35, their jobserver tokens died with them, and the build wedged with
  seven waiters frozen on the empty jobserver pipe
  (`bytes=0 writers=7`, futex `queued_for=1164 s`). **The "live-but-idle
  rustc" wedge — open issue #2 of the handoff — is downstream of the
  instruction-abort class, not of the scheduler races.** Same-day findings
  on that class (musl's RELR `+= base` loop at `_dlstart+0x12c`, `DT_RELR`
  in Alpine's ld-musl, no kernel RELR handling, global-not-per-file N, live
  busybox reproducer) are in the runbook's "second, separate crash" section.

## Methodology notes

- The fix candidates came from **enumerating writers** (all `msr ttbr0`
  sites, all `THREAD_STATES` READY stores), not from staring at the fault
  dump. One fault is not a distribution; the writer list is exhaustive.
- The corruption was only explicable after asking *"what does the saved
  context contain when the slot is revived?"* — the switch path itself was
  provably fine, which is what pointed away from it.
- A tripwire at the moment of corruption (switch time) is worth more than a
  richer fault dump (megabytes and one context switch too late).
- The `[SGI-S STACK]` noise almost buried the investigation: 20k false
  positives per boot from a diagnostic added in a previous session. Gate
  diagnostics against known-legitimate populations (idle threads) before
  trusting their volume as signal.
