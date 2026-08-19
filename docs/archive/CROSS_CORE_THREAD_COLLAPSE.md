# Cross-core "slow memory" investigation: the same-process thread collapse

**Date: 2026-08-19.** Follow-up to `BENCHMARK_PERFORMANCE_ATTEMPT_0.md`, which
measured llama.cpp decode (`tg`) collapsing **22x when going `-t 1` -> `-t 2`**
(36 -> 1.6 tok/s) and ~200x at `-t 4`, while Redis (separate processes) scaled
near-linearly across cores. The starting hypothesis — "cross-core memory access
passes through extra boundaries left over from the deleted multikernel" — is
**refuted**: there are no extra copies, mappings, or unmappings on the path, and
user memory attributes are correct (Normal WB, inner-shareable, `nG` on every
user PTE; verified in `crates/akuma-exec/src/mmu/`). Memory is not slow.

**Final outcome (read this first):** three landed fixes took decode from
36 / 1.6 / 0.28 / 0.18 tok/s (`-t 1/2/3/4`) to **45.6 / 68.2 / 96.3 / 6.6** —
real multi-thread scaling, with `-t 1` now above the Docker/Linux reference
(40.6). The dominant root cause, found last (§0), was **EL0 counter reads
(`CNTVCT_EL0`/`CNTFRQ_EL0`) trapping at ~1M pairs/s and being emulated as 0**;
the per-switch full TLB flush (§1) and the scheduler's displacement behavior
(§2) were real amplifiers on top. `-t 4` remains oversubscribed by design
(4 compute threads + the netpoll core on 4 cores).

## 0. THE ROOT CAUSE: CNTVCT/CNTFRQ traps at ~1M pairs/s, emulated as zero

Found by decomposing the "~1.3M exception-vector entries/s per core during
decode" storm (§3): new `[EXCC]` per-class + per-`ESR.EC` counters showed it
was **sync EL0, EC=0x18** (trapped MRS), and the one-shot encoding counters
named the registers: `op0=3 op1=3 crn=14 crm=0 op2=2` (**CNTVCT_EL0**) and
`op2=0` (**CNTFRQ_EL0**), perfectly paired — a userspace timing read, ~1M
pairs/s under decode (ggml/OpenBLAS spin- and poll-loops time themselves on
every iteration).

Two compounding defects:
1. `CNTKCTL_EL1` was never programmed, so EL0 counter access stayed trapped.
   Linux sets `EL0VCTEN` unconditionally; Akuma paid a full EL0->EL1->EL0
   round trip per read — ~30-80% of every core under decode.
2. The EC=0x18 emulation (`src/exceptions.rs`) returned **0** for every
   register except CTR_EL0 — so userspace's hardware clock was FROZEN AT ZERO.
   ggml's spin budgets and park/wake heuristics ran on a dead clock, which is
   what actually turned barriers pathological.

**Fix:** `CNTKCTL_EL1 = 0b10` (EL0VCTEN) at BSP boot (`boot.rs
configure_mmu_regs`) and secondary bringup (`smp_shared.rs` trampoline), plus
real-value fallbacks for CNTVCT/CNTFRQ in the trap emulation. After it, EC=0x18
disappears from `[EXCC]` outright and the per-core exception rate under decode
drops to ~3K/s (timer + SGIs + syscalls).

**Context switching and scheduling around it were also defective.** One real
defect was found and fixed (§1), the scheduler's displacement behavior was
reworked (§2), and two scheduler experiments were run to a measured verdict
each.

All numbers: devbox-smoltcp (`smp-shared`), SMP=4, MEMORY=4096, QEMU HVF on
Apple Silicon (`-cpu host` — real hardware TLBs), qwen3.5-0.8b-q4 (532 MB,
mmap'd), `llama-bench -p 0 -n 16` unless noted.

## 1. FIXED: unconditional full TLB flush on every context switch

`sgi_scheduler_handler_with_sp`'s switch tail executed

```
dsb ish; msr ttbr0_el1, <new>; isb; tlbi vmalle1; dsb ish; isb
```

**unconditionally** — including when the incoming thread's TTBR0 equals the
live one (sibling threads of one process; all kernel threads, which share the
boot table). `tlbi vmalle1` flushes ALL ASIDs on the local core, and the kernel
itself runs in the TTBR0 low half, so every switch amputated the kernel's
translations too. It was the correctness crutch for "every address space uses
ASID 0" (`smp-shared.md` M3). With the 1 ms tick, any core alternating between
two threads paid a full TLB + walk-cache rebuild up to 1000x/sec — over a
532 MB working set at 4 KB granularity. `-t 1` never switches on its core, so
it never paid; `-t 2` paid constantly. That asymmetry is what made "memory" look
cross-core-slow.

**Fix** (`crates/akuma-exec/src/threading/mod.rs`, the SGI switch): skip both
the `msr` and the flush when `new_ttbr0 == current_ttbr0`. Safety argument, in
the code comment at the site:

- An equal value is provably the SAME address space, not a recycled L0 frame:
  the live-TTBR0 free gate (`mmu::any_core_on_l0` + the saved-context arm)
  refuses to free an L0 while this core's `ACTIVE_L0` names it, so the frame
  cannot be freed and re-issued while the core stays on it.
- Staleness is covered: page-table edits broadcast (`tlbi ...is`) under
  `kernel_smp_shared` (M3), and off smp-shared the local core made and flushed
  its own edits.
- Cross-AS switches keep the full `vmalle1` — that is where ASID 0 genuinely
  needs it. (Follow-up candidate, NOT landed: `tlbi aside1, #0` on cross-AS
  switches would preserve global kernel entries; every user PTE already
  carries `nG`. Needs an audit that kernel/global mappings are attribute-
  identical across the boot table and every per-AS `add_kernel_mappings`.)

**Regression test**: `test_same_ttbr0_switch_pingpong` (src/process_tests.rs) —
two threads sharing the boot TTBR0 ping-pong a token through shared atomics
with a yield per hand-off; 400 hand-offs through the skip path. Full boot suite
at SMP=4: 291 PASSED, 0 FAILED, no `[TTBR *-MISMATCH]`, no `[SGI-S *]`.

**Measured effect**: `tg16 -t 2` 1.61 -> 2.5–4.2 tok/s (~2x). `-t 1` and
`-t 3/-t 4` unchanged. Real, but NOT the dominant cost.

## 2. The dominant remaining cost: barrier hand-off latency, not memory

With the flush fixed, `-t 2` still ran ~10-14x below `-t 1` while both compute
threads held dedicated cores (QEMU steady at ~303% host CPU — full burn, no
progress). The guest's own per-process syscall ring (`/proc/<pid>/syscalls`)
during a live `-t 2` decode showed the llama main thread completing **only
`futex` calls, each taking 0.2–5 ms**, while the worker threads made **zero
syscalls** (pure EL0 spin). ggml's barrier path degrades to futex park/wake as
soon as the partner is off-CPU longer than the spin budget, and each park/wake
round trip cost ~a scheduler tick or more. Hundreds of barriers per token
x ~1 ms each = the collapse.

Why is the partner off-CPU at all, with free cores? Three scheduler behaviors
stack up:

1. **The round-robin displaces working threads for any READY thread.** The
   scan (`schedule_indices`) never keeps the current thread when any other
   non-idle READY thread exists, and nothing routes READY work to idle cores
   first (idle cores sleep in WFI until their own 1 ms tick; `wake_core` fires
   only from `ThreadWaker::wake`).
2. **The idle-fallback displaced even the LAST working thread.** When the scan
   found no other READY thread, it switched the current RUNNING thread out to
   the per-core idle thread anyway; another core's tick picked the thread back
   up ~1 ms later. Every CPU-bound thread bounced core -> idle -> other core
   every tick (idle threads billed ~39% CPU each). Fixed: the fallback now
   returns `None` (keep running) when the current thread is still RUNNING —
   safe because a RUNNING thread is never picked by a peer's scan. Measured
   effect on llama: none on its own (at `-t >= 2` there is always another
   READY thread so the fallback wasn't the llama path), but it removes the
   bounce for every single-busy-thread regime and is strictly better.

   **CRITICAL LESSON, found the hard way**: the first version applied this
   (and §2B) to EL1-interrupted threads too, and the boot suite promptly
   wedged in a `[BKL] stuck` storm at the FS tests — under smp-shared the BKL
   is held iff a core is in EL1, and the per-tick switch to idle (whose loop
   drops the BKL around WFI) was the RELEASE VALVE for long kernel excursions.
   Both mechanisms are therefore gated on the tick having interrupted **EL0**
   (SPSR.M[3:0]==0 read from the IRQ frame, threaded through
   `schedule_indices(voluntary, core_id, interrupted_el0)`). Userspace holds
   no BKL, which is exactly the llama case, so the gate costs the win nothing:
   with it, boot suite 291 PASSED / 0 FAILED with FEWER transient
   `[BKL] stuck` lines than the pre-change baseline run (91 vs 101).
3. **A woken thread joins the BACK of the rotation.** `ThreadWaker::wake` CASes
   WAITING->READY and rings SGIs, but the receiving scheduler round-robins;
   the futex-woken barrier partner waits out the rotation behind the netpoll
   thread and friends. This is the same "eligibility is not execution"
   short-sleep floor the 2026-08-18 `WAKE_DEADLINE_PREEMPT` fixed for timer
   wakes, still open for explicit wakes.

### Experiment A — arm the run-next hint from explicit wakes: WORSE, reverted

`WAKEUP_LOCALITY_HINT = true` (the existing `PREEMPT_WAKE_TID` mechanism, off
since the rump-sysproxy measurements): `tg16 -t 2` **2.5 -> 1.15 tok/s**. The
single-slot hint preempts whichever thread the receiving core runs — regularly
the producer that still had compute left. Reverted; the negative result is now
recorded on the const's doc comment.

### Experiment B — displacement bypass toward idle cores

New mechanism: on an involuntary tick that would displace a RUNNING, non-idle
thread, first try `wake_remote_idle()` (now returns whether an idle peer was
found and SGI'd); if a peer takes the work, this core keeps its thread.

- **Unbounded version: catastrophic for interactivity.** Spinning llama
  threads gained indefinite immunity whenever any core was idle; sshd and the
  netpoll maintenance work rotated on one leftover core. 973 preemption-
  watchdog warnings, `[SGI] POOL contended` climbing, ssh handshakes dropped
  ("Connection closed" on connect) during `-t 3` cells.
- **Bounded version (landed)**: a thread may decline displacement at most 4
  consecutive ticks (`DISPLACEMENT_IMMUNITY_TICKS = 5`, i.e. an effective
  ~5 ms timeslice), then rotates exactly as baseline. Queued READY threads
  wait a bounded few ticks. **This is the big win**: `tg16 -t 2` jumped to
  **21.8–22.3 tok/s** (13.5x over baseline; ~60% of `-t 1`), `-t 3`/`-t 4`
  improved 3–6x, `-t 1` unchanged — with **zero** preemption-watchdog hits
  and ssh interactive through the whole sweep. The per-core exception rate
  under decode also fell ~1.3M/s -> ~140K/s (fewer displacement switches =
  fewer SGI/switch cascades).

## 3. Side findings (each independently actionable)

- **File-page-cache thrash under mmap'd models**: `[FPCACHE]
  entries=131072/131072 evict_mapped=20124` — the cache cap is 512 MB
  (131072 x 4 KB); the 532 MB model + binaries exceed it, so **mapped weight
  pages are evicted and re-faulted continuously** (llama at `-t 1`:
  pgfault=167268 / 302065 pages in 63 s ≈ 2.6 K faults/s steady-state). This is
  the leading suspect for the remaining ~13% single-thread decode gap vs
  Linux (`BENCHMARK_PERFORMANCE_ATTEMPT_0.md` §"decode"), and it charges every
  thread count.
- **The netpoll thread occupies ~93-97% of one core permanently**
  (`cpu_us` 1596 s of a 1710 s boot). Under `smp-shared` its loop WFIs with the
  BKL dropped each iteration, so part of that is *billed* wait, but host-side
  CPU accounting during `-t 2` (303% for 2 compute threads + net) shows ~a
  full vCPU genuinely burning. At SMP=4 that is 25% of the machine.
- **~1.3M exception-vector entries per second PER CORE during decode** (from
  `[EXC]`, idle baseline ~700/s), at every thread count including `-t 1`, on
  all four cores equally. Decomposition via the new `[IRQS]` per-INTID counters
  (`exceptions::IRQ_BY_INTID`): SGI 0 + timer PPI 27 account for only ~8.6 K/s
  TOTAL — so the storm is **sync exceptions, not IRQs**, yet PSTATS shows only
  ~300 syscalls/s + ~2.6 K faults/s. ~5M/s of vector entries are unattributed.
  **OPEN** — next probe: split `EXCEPTION_ENTRIES` by vector class (sync-EL0 /
  sync-EL1 / default) the same way `IRQ_BY_INTID` split the IRQs.
- **Two diagnostics found lying during this hunt**:
  - `fe()` (the futex event ring, `src/syscall/sync.rs`) is a racy
    load-shift-store; concurrent events (a waker's `W` vs the waiter's `p`)
    silently vanish, so `[FUTEX-ORPHAN] hist=` tails are unreliable evidence.
  - `futex_orphan_check` snapshots the queued-tid list ONCE before scanning
    thread states, so a thread that re-enqueued mid-scan reports as a false
    orphan. The `[FUTEX-ORPHAN]` lines seen during llama runs are consistent
    with these artifacts plus ordinary wake-latency, NOT with a proven
    lost-wake kernel bug.
- The multikernel-legacy hypothesis is fully dead: no bounce buffers, no
  forwarding, no per-core memory boundaries exist on the smp-shared memory
  path (they were removed with the multikernel, `TRIM_FAT_MULTIKERNEL.md`).

## 4. Results

`tg16` tok/s (`-p 0 -n 16 -r 1`, mmap=1 arm shown; Docker/Linux reference from
`BENCHMARK_PERFORMANCE_ATTEMPT_0.md`: `-t 1` 40.6):

| kernel | `-t 1` | `-t 2` | `-t 3` | `-t 4` |
|---|---:|---:|---:|---:|
| before (2026-08-19 baseline) | 36.0 | 1.61 | 0.28 | 0.18 |
| + same-TTBR0 switch skip (§1) | 35.2 | 2.5–4.2 | 0.26 | 0.17 |
| + wake hint ON (§2A, reverted) | 35.1 | 1.15–1.23 | 0.30 | 0.20 |
| + keep-RUNNING fallback (§2.2) | 35.2 | 2.6–3.8 | 0.26 | 0.17 |
| + bounded displacement bypass (§2B, ungated) | 34.3–36.9 | 21.8–22.3 | 1.5–1.7 | 0.46–0.47 |
| + EL0 gate on §2B/§2.2 (BKL valve, required) | 39.3–40.4 | 5.6–6.0 | — | 0.18 |
| **+ CNTKCTL EL0 counter access (§0, final)** | **44.8–45.6** | **67.9–68.2** | **73.7–96.3** | **5.8–6.6** |

Reading the table: the ungated bypass "won" `-t 2` largely by shielding threads
that were in EL1 **because of the §0 trap storm** — once the storm is gone,
threads genuinely run in EL0, the (mandatory) EL0-gated bypass protects them,
and the collapse inverts into real scaling. `-t 4` stays oversubscribed
(4 compute + netpoll > 4 cores; open item). Raw CSVs:
`logs/llama_bench/akuma_tg_tsweep*.csv` (`_fixed` = §1 only, `_wakehint` = §2A,
`_keeprun` = §2.2, `_bounded` = ungated §2B, `_final` = EL0-gated pre-§0,
`_cntvct` = final).

## 5. What remains open (ranked by expected value)

1. ~~Attribute the 5M/s sync-exception storm~~ — **DONE, it was §0** (CNTVCT/
   CNTFRQ traps), found via the `[EXCC]`/`mrs.` counters this investigation
   added. The counters stay in the heartbeat; a reappearing `e0.0x18` bucket
   means some new register joined the party.
2. **`-t 4` oversubscription**: 4 compute threads + the netpoll thread on 4
   cores. The netpoll core tax (~93-97% of one core, `cpu_us`-billed WFI
   included) is the other half of this item. Real fix candidates: per-core run
   queues + affinity (M5-class), or a cheaper netpoll idle mode.
3. **FPCACHE sizing/policy** for mmap'd files larger than the cap — mapped-page
   eviction guarantees a steady refault tax on exactly the pages a decoding
   model streams. Candidates: raise the cap under high-RAM boots; exempt
   actively-mapped pages from eviction; per-file working-set hints.
4. **Explicit-wake latency**: a futex-woken thread should run within SGI
   latency on SOME core, without preempting the producer (what killed
   Experiment A). Per-core hint slots, or wake-to-idle-core-first routing.
   Much less urgent post-§0 (barriers rarely reach the futex path now).
5. Cross-AS `tlbi aside1, #0` (kernel-global-preserving flush) — §1 follow-up.
6. Fix the two lying diagnostics (§3) — `fe()` needs an atomic RMW ring;
   the orphan check needs a per-tid re-check under the table lock.
7. Re-run the Redis matrix: the §0 trap tax and §1 flush were also present
   under every Redis measurement in `BENCHMARK_PERFORMANCE_ATTEMPT_0.md`; the
   "9x per-round-trip" number likely shrinks.

## Background

- `BENCHMARK_PERFORMANCE_ATTEMPT_0.md` — the benchmark harness, fairness
  controls, and the numbers that opened this investigation.
- `PAGE_TABLE_UAF_BKL_STORM.md` — the live-TTBR0 registry / free gate this
  fix's safety argument leans on.
- `SCHEDULING_INVESTIGATION.md` — the short-sleep floor and
  `WAKE_DEADLINE_PREEMPT`, the deadline-wake half of §2.3.
- `J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md` §7.9 — the earlier
  lost-wake evidence the 200 ms futex revalidation backstop answers; this
  session's "orphans" are NOT confirmed instances of it (see §3).
- `DEVBOX_ISSUES.md` Issue 22 — the cross-box memory-isolation verification
  task filed alongside the §1 change.
