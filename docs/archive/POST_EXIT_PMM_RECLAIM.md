# Post-exit PMM reclaim — is there a leak at the low-memory floor?

**Question (2026-06-05):** at the extreme low-memory floor (the 4.5–5 MB
meow+tcc region) free PMM seems not to recover after a process exits, so a later
spawn / demand-fault hits "0 free pages". Are dead processes leaving artifacts
behind (un-freed user pages / page tables / heap)?

**Answer: no per-process leak.** The single-process teardown path conserves
physical memory exactly. The "never recovered" symptom is a *one-time working-set
step* (the warm thread-stack floor + VFS caches), not a ratchet — and at 4.5 MB
the binding constraint is raw PMM scarcity, not a leak.

## How the teardown frees memory (code trace)

A dying process returns its frames through, in order:

- `return_to_kernel(-N)` (normal exit, and the OOM-SIGSEGV path,
  `exceptions.rs` data/inst abort → `return_to_kernel(-11)`) →
  `unregister_process(pid)` → drops `Box<Process>`.
- `Process::drop` frees `dynamic_page_tables`.
- `UserAddressSpace::drop` frees `user_frames` (each distinct PA once — a
  *mapping* refcount, not an alloc count), `page_table_frames`, and `l0`.
- `kill_process` / `kill_process_with_signal` instead mark the process **Zombie**
  and defer the same Drop to reaping (`on_thread_cleanup` → `unregister_process`
  when the terminated thread slot is recycled).

`mmap` frames are *aliased* in `user_frames` (`syscall/mem.rs` calls
`track_user_frame`), so AS Drop frees them; `Process.mmap_regions` is just
bookkeeping (`PhysFrame` is `Copy`/no-`Drop`, so dropping the Vec frees no pages —
and that is correct, not a leak).

## Evidence (page-precise)

Self-test `test_pmm_conserved_across_spawn_exit_reap` (src/process_tests.rs):
spawns a real `/bin/hello`, drives it through exit **and** kill, forces reap
(`threading::cleanup_terminated_force()`) and `allocator::reclaim_to_pmm()`, and
asserts `pmm::free_count()` does not ratchet down across repeated cycles. Result
at MEMORY=64M:

```
clean 4x drift=0p; kill 4x drift=0p; pinnedspans 0->0
```

Live reproduction, extreme-size kernel at MEMORY=6M, page-precise `[Mem]` line:

```
baseline                         RAM free 3100KB
after 20 spawns (round 1)        RAM free 2972KB   <- one-time step
after 20 spawns (round 2)        RAM free 2972KB   <- flat (no ratchet)
after 20 spawns (round 3)        RAM free 2972KB   <- flat
after forktest_parent            RAM free 2972KB   <- fork/CoW path also recovers
```

The 128 KB step is the warm thread-stack floor (lazy stacks keep
`WARM_FREE_USER` stacks allocated by design) plus retained VFS read-ahead of the
spawned binaries — a stable working set, reclaimed-stable, not growing.

## Diagnostics added (keepers)

- **`allocator::claimed_span_report() -> SpanReport`** + a `spans:` field on the
  `[Mem]` line: how much PMM the kernel heap is sitting on and how much is *stuck*
  (`pinned` = spans Talc can't return because one live allocation pins them).
  This is the real signal for the kernel-heap **high-water mark**: a span only
  returns to the PMM when it is *entirely* free, so fragmentation by long-lived
  allocations is what would keep `pinned` high after a heavy heap workload.
- **Page-precise free RAM** (`(NNNNKB)`) on the `[Mem]` line — the MB figure
  can't show sub-MB recovery, which is exactly the floor symptom.

## What the floor symptom actually is

At these sizes the kernel heap never grows past its tiny seed (6 MB: heap "used"
~134 KB, 0 claimed spans), so the heap high-water / `reclaim_to_pmm` path is not
even exercised — it only bites under genuinely heap-heavy load (many sockets,
big compiles). The 4.5 MB meow+tcc floor is bounded by the **combined user-page
working set vs available PMM**, not by leaked artifacts. To push the floor lower,
shrink the working set (warm-stack floor, per-process metadata), not chase a
leak that isn't there.

If `pinned` is ever observed staying high after a heap-heavy workload drains,
*that* is the high-water bug and the lever is fragmentation (segregate
process-lifetime heap allocations so freed spans become wholly free again).

---

# REOPENED 2026-09-07: `retired_reclaim_ab` fails on `main`, everywhere

**Status: open, unowned, and NOT caused by the amd64 scheduler fold** — that is
the whole point of the matrix below. It was found while establishing an aarch64
control for `docs/archive/AKUMA_AMD64_BLOCKING.md`, i.e. by looking for a
regression and finding a standing failure instead.

## What fails

```
[FAIL] retired_reclaim_ab: parked 1024p, OFF recovered 0p (retired 1),
       ON recovered 0p (retired 1)
       — expected OFF to strand (<256p) and ON to recover (>=512p)
```

The A/B in `src/process_tests.rs` parks a 1024-page address space, then measures
recovery with pressure-driven retired-process reclaim **off** (A) and **on** (B).
A passing run wants A to strand the memory and B to recover it. Observed: **both
sides recover 0p, and both leave one slot RETIRED.** The A side is behaving as
designed. **The B side is the failure**: the mechanism it exists to demonstrate
does not fire.

## The matrix — this is the part that matters

Every cell is one boot, `SMP=1`, same disk, same day.

| kernel | accelerator | MEMORY | PASSED | `retired_reclaim_ab` |
|---|---|---|---|---|
| `main` @ b7c89d47 | HVF | 2048M | 307 | **FAIL** |
| branch @ dbbbb986 (pre-fold) | HVF | 2048M | 307 | **FAIL** |
| branch, post-fold | HVF | 2048M | 307 | **FAIL** |
| `main` @ b7c89d47 | KVM (Lima) | 256M | 305 | **FAIL** |
| branch, post-fold | KVM (Lima) | 256M | 305 | **FAIL** |

Identical on `main`. Identical across two accelerators and an 8× memory range.
So it is neither a branch regression nor an environment artifact, and the three
kernels are otherwise **bit-for-bit equivalent in outcome** (307/307/307, same
single failure) — which is also the evidence that the scheduler fold changed
nothing on aarch64.

## The clue worth starting from

`retired_reclaim_pressure_rung` **passes immediately before it**, in the same
boot, recovering real memory:

```
[PASS] retired_reclaim_pressure_rung: parked 512p,
       free 392799 -> 392282 -> 392799 (517 recovered, 1 slot freed by the rung)
```

So the reclaim rung works. What does not work is the A/B's **B side** reaching
it. Two hypotheses, in order of cheapness:

1. **No pressure, so no pressure-reclaim.** The rung is pressure-gated, and at
   2048M there are ~392 000 free pages — parking 1024 of them is 0.26% of RAM.
   The `pressure_rung` test drives the rung directly; the A/B waits for it to
   fire on its own. If that is it, the A/B is testing "does pressure occur",
   not "does reclaim work", and the fix is to the test.
   **Against this hypothesis:** it fails at 256M too, where 1024p is ~1.6% —
   still possibly not enough. Worth measuring what the threshold actually is
   before assuming.
2. **The retired slot is not eligible.** Both sides report `retired 1`, so a
   slot *is* retired and *is not* being taken. `reclaim_retired_processes_force`
   is called in the teardown after sampling, so whatever holds it is held at
   sampling time.

## What the test already knows about itself

The comment above the assertion is worth reading before touching anything: the
bar was moved to `PARK / 2` after 12 boots showed the ON side is **strictly
bimodal, 1029p or 745p, never between** — the ~284-page difference being
`/bin/hello` sitting as an ACTIVE zombie awaiting a `wait4` the test never
performs. **0p is a third mode, outside that recorded set**, so this is not the
same sampling noise the bar was widened for. Something else changed, or the
bimodality was never the whole story.

Note also: waiting does not help. A 500 ms "wait for `free_count` to stabilise"
loop was measured to change neither outcome.

## Two other failures seen alongside, and their status

Both also reproduce on `main`, so neither is a branch regression:

- **`test_mmap_file_oom_survives`: "PMM not reclaimed after kill (500 polls)"**
  — `before=33247 after=23680`. Only reachable at small RAM: at 2048M it
  `[SKIP]`s ("no /models file larger than RAM"). So this is a *different memory
  configuration*, not an accelerator artifact, and it is a second, independent
  post-kill reclaim failure. It may share a cause with the above; it may not.
- **`test_epoll_socket_waker`: "latency too high (10338–11370 us)"** — seen only
  under Lima/KVM, on `main` too. ~10 ms is suspiciously exactly one timer tick,
  which suggests a wake being served by the tick rather than by the waker in
  that environment. Lowest priority of the three; likeliest to be the rig.

## Reproducing

```bash
cargo build --release
# on the laptop, under HVF — MEMORY=2048M is required, see below
INSTANCE=8 MEMORY=2048M sh scripts/cargo_runner.sh \
    target/aarch64-unknown-none/release/akuma
# or in Lima under KVM (defaults to 256M, which also runs the mmap test)
limactl shell fc sh scripts/lima_aarch64_run.sh
```

**`MEMORY=2048M` under HVF is not optional**: below 2048M this suite dies with
`Assertion failed: (isv) ... hvf.c` and QEMU exit 134, which is the
configuration and not a kernel bug (`scripts/cargo_runner.sh` prints a warning
saying so; `docs/archive/QEMU_HVF_ISV_BUG.md` "Root cause 5"). That assertion is
easy to mistake for a crash introduced by whatever you are testing — it is not.

## Background

- The body of this document (2026-06-05) answers the *original* question — the
  single-process teardown path conserves memory exactly, and there is no
  per-process leak. Nothing below contradicts that; the retired-slot reclaim
  path is a different mechanism, added later.
- `docs/archive/OOM_KILL_DEFERRED_RECLAIM_GAP.md` — the gap the A side exists to
  demonstrate.
- `docs/archive/BOOT_SUITE_PMM_DEFERRED_RECLAIM.md` — why these tests have to
  force `cleanup_terminated_force` + `reclaim_retired_processes_force` by hand.

---

# Root cause found 2026-09-07: 7ec02e91's TERMINATED gate disarmed every teardown drain site

## The mechanism, and where it broke

`retired_reclaim_ab` has exactly one collector that can run at the moment it
samples: the exit-path teardown drain (`drain_retired_before_parking` in
`akuma-syscalls-glue/src/proc.rs`, twin call in `return_to_kernel` /
`return_to_kernel_from_fault` in `akuma-exec/src/process/mod.rs`). At SMP=1
during the boot suite every other collector is absent:

- `netpoll_maint` (`akuma-kernel-glue/src/lib.rs:2136`, intact) — not spawned yet;
  the suite runs before `run_async_main_preemptive`.
- thread 0's idle loop (`akuma-kernel-glue/src/lib.rs:1334`) — thread 0 *is* the
  test, not idling.
- the `smp-shared` per-core idle loop (`akuma-entry/src/smp_shared.rs:634`) —
  no secondaries at SMP=1.
- `drain_retired_under_pressure` (the PMM ladder) — declines: at 2048M parking
  1024p is 0.05% of RAM, no allocation failure ever occurs.

Commit **7ec02e91** ("fix reclamation of the heap", 2026-09-03) added to the top
of `drain_retired` (`crates/akuma-exec/src/process/reclaim.rs`):

```rust
if crate::threading::current_thread_is_terminated() {
    request_retired_reclaim();
    return 0;
}
```

All three teardown drain sites call it **immediately after** marking their own
thread terminated — the condition is always true there *by construction*. The
gate turned all three sites into no-ops, and the A/B's B side began waiting for
a collector that no longer exists: both sides measure `0p`, one slot stuck
RETIRED. (The gate's own comment says "terminal sites keep requesting" — true,
and useless: a request flag needs an eligible collector to ever be serviced.)

**Verified experimentally** (worktree, gate disabled, one boot, HVF 2048M):
`retired_reclaim_ab` PASSes — `OFF recovered 0p (retired 1), ON recovered 740p
(0 left)`; 740p is inside the documented 745/1029 bimodal band. Neighbouring
tests already prove the retire/stamp/cooldown half works when any live
collector calls the drain (`retired_reclaim_pressure_rung`,
`retired_reclaim_request_flag`).

The gate itself fixed a real bug (`SELFHOST_KERNEL_HEAP_LEAK.md`): a TERMINATED
thread can be reaped at any yield and never resumes, so a sweep it started is
abandoned mid-`Process::drop`, stranding that address space's `user_frames`
map. **Do not simply delete the gate.** Replace it with a pin.

## The fix: reap-safe drain (sketch)

Keep the gate's hazard model — a terminated drainer must never be reaped
mid-sweep — but pin the reaper instead of skipping the drain. Three parts:

### 1. `drain_retired` (akuma-exec/src/process/reclaim.rs)

Replace the unconditional bail with a *pinned* sweep for the terminated case:

```rust
let terminal = crate::threading::current_thread_is_terminated();
if terminal {
    // A dying thread must finish what it starts: with preemption disabled it
    // cannot be switched away mid-sweep, and it never yields voluntarily, so
    // the sweep is guaranteed to complete and DRAINING[tid] is guaranteed to
    // clear. No abandonment, no stale pin.
    akuma_primitives::preempt::disable_preemption();
}
// ... existing DRAINING[tid] reentrancy guard + sweep + flag recompute ...
// (restructure the early `return 0` arms so the enable_preemption() below
// always runs — RAII or a single exit path.)
if terminal {
    akuma_primitives::preempt::enable_preemption();
}
```

`disable_preemption`/`enable_preemption` already exist in
`akuma-primitives/src/preempt.rs` with watchdog diagnostics. Note the sweep
site runs with IRQs enabled, and the disabled window is bounded by the free of
the parked address spaces — a dying thread's last act; the 100 ms watchdog line
is diagnostic-only and acceptable here (the pre-7ec02e91 behaviour drained this
same work from this same site while holding the BKL, and that was the shipped
design for months).

### 2. Reaper pin (akuma-threading/src/lib.rs, `cleanup_terminated_internal`)

Next to the existing `ON_CPU[i]` guard (~line 2124) — which already skips a
slot whose core may still be on its stack — add the drain pin:

```rust
// A terminated thread may be running its final retired-process sweep
// (process::reclaim). Don't free its stack out from under it; the next
// pass gets it. With preemption disabled for the sweep this cannot fire,
// but it is the same belt the ON_CPU check is: cheap, and it keeps the
// invariant local to the reaper rather than trusting every drainer.
if (runtime().drain_in_flight)(i) {
    continue;
}
```

### 3. Hook wiring (four one-line edits)

- `akuma-threading/src/lib.rs` runtime table (~line 185): add
  `pub drain_in_flight: fn(usize) -> bool` beside `clear_draining`.
- `akuma-exec/src/lib.rs` (~line 223): wire
  `drain_in_flight: process::reclaim::drain_in_flight` — a new
  `pub(crate) fn drain_in_flight(tid: usize) -> bool` reading `DRAINING[tid]`.
- Host stub `akuma-threading/src/test_support.rs` (~line 72): `|_| false`.
- amd64 stub `amd64/src/sched.rs` (~line 368): `|_| false`.

### Why the interleaving is now impossible

The reaper only reaps a TERMINATED slot when it is off-CPU (`ON_CPU[i] == 0`).
A drainer mid-sweep is either (a) on-CPU — ON_CPU already excludes the reaper —
or (b) was preempted — impossible, the sweep runs preempt-off and never yields.
So the reaper can never observe a drainer's slot in a reapable state while the
sweep is live, DRAINING always clears (guaranteed completion), and the slot is
always reaped on a later pass. The pin flag therefore cannot go stale — which
is the one way a naive DRAINING-only version of this fails (an abandoned sweep
would leave the flag set forever and strand the stack with it).

### Expected outcome

- `retired_reclaim_ab` B side recovers through the exit-path drain again
  (measured 740p with the gate naively disabled; the pinned version should
  match, 745/1029 bimodal band).
- The `SELFHOST_KERNEL_HEAP_LEAK.md` abandonment residual stays fixed — the
  sweep is now *never* abandoned rather than never *started*.
- `test_mmap_file_oom_survives` (256M failure in the matrix above) is worth a
  re-run after the fix: it is a second post-kill reclaim failure and may share
  this root cause, since a SIGSEGV kill also exits through a terminal drain
  site.
