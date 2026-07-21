# SMP Go combined_stress: phantom-SVC corruption + BKL teardown wedge — Investigation Prompt

## Problem Statement

With the waitid/pidfd fix in (see `debug-forktest-go-hang.md`), the Go forktest
runs under shared-kernel SMP — and immediately exposes two kernel bugs:

1. **Phantom-SVC exception misclassification** (silent data corruption, present
   from SMP=2 up).
2. **Hard BKL wedge at the SIGTERM/teardown phase** (SMP=4).

## Repro

```bash
SMP=4 overlays/devbox/run-smoltcp.sh    # devbox-smoltcp, release-smp-shared
ssh -p 2222 root@localhost '/bin/forktest_parent -num_children=3 -duration=15s -combined_stress'
```

Scaling (same kernel, one run each, 2026-07-22):

| SMP | `[SPURIOUS-SVC]` | `WILD-DA` | outcome |
|-----|-----------------|-----------|---------|
| 1   | 0               | 0         | PASS |
| 2   | 8               | 0         | PASS — **but silently corrupting** |
| 4   | 25              | 2         | one child SIGSEGV, then hard BKL wedge at the 15 s deadline (~7.5k `[BKL] stuck: owner=2`, 0 RECOVERED, 0 WATCHDOG) |

Reference logs from the session that found this: `forktest_smp{1,2,4}_fixed.log`,
`forktest_smp2_final.log` (repo root).

## Evidence chain (bug 1 — phantom SVC)

Typical storm entry:

```
[SPURIOUS-SVC] stale-icache: nr=10200780808 elr=0x86840 insn@elr-4=0xcb050021 (not svc) x0=0x261bed000 — IC flush + replay #2
...
[SPURIOUS-SVC] giving up at elr=0x86840 after 20 replays — dispatching nr=113
```

Disassembly of `userspace/forktest/forktest_child` at the two recurring ELRs:

- `0x86840` = **`dc zva, x0`** inside `runtime.memclrNoHeapPointers` (Go bulk-zeroing
  freshly allocated spans).
- `0x45db4/0x45db8` = **`ldar x0,[x2]` / `ldrsb x27,[x0]`** in `runtime.(*spanSet).push`.

Pattern decode:
- `x0` is always a **page-aligned, fresh Go-heap address** → these PCs take
  **demand-paging data aborts** (first touch of a lazy page), constantly and legally.
- ELR points **at** the instruction (data-abort semantics), so `insn@elr-4` is
  never an `svc` — a real SVC would have ELR = svc+4.
- "nr" is whatever `x8` last held: 113 (`clock_gettime`, Go's hottest real
  syscall) or a Go heap pointer (0x2_60xx_xxxx ≈ 10.2e9). i.e. **no syscall setup
  ever happened** — the trap is not an SVC.

So: an EL0 **data abort is being classified as EC_SVC64**. It is NOT stale
icache (the guard's original theory): IC flush + replay does not heal it at the
same ELR (up to 20 replays), and the sites are data, not code, dependent.

### Prime suspect (code-confirmed window, not yet fix-verified)

`src/exceptions.rs`:

- `rust_sync_el0_handler` (the BKL wrapper, ~line 2117) calls
  `bkl::enter_kernel()` **first**. Under contention this spins with **IRQs
  enabled** (deliberate — see M2c bug #2 in `docs/archive/SMP_SHARED.md`), so a
  timer SGI can nest, context-switch, run other threads (whose traps overwrite
  the per-PE `ESR_EL1`/`FAR_EL1`), and/or resume this thread on a **different
  core** whose ESR holds a stale SVC syndrome.
- `rust_sync_el0_handler_inner` (~line 2128) only THEN reads `mrs esr_el1` and
  classifies the exception.

Reading `ESR_EL1`/`FAR_EL1` after any preemptible window is unsound: the
syndrome registers are per-PE state valid only until the next trap on that PE.
This fits the scaling perfectly — the misclassification rate tracks BKL-spin
time (zero uncontended at SMP=1).

Note: something already captures an `entry_esr` "at the very top of
rust_sync_el0_handler" (used near line 992 for an interrupted-syscall check) —
find where that snapshot happens and why the inner handler still re-reads live.

### Corruption amplifier (fix regardless of root cause)

The guard's give-up path **dispatches the garbage nr anyway**
(`giving up ... — dispatching nr=10201055240`). That writes ENOSYS into `x0` of
a thread that was mid-`memclr`/`spanSet.push` with a live pointer in `x0` →
guaranteed register corruption → the observed downstream `WILD-DA FAR=0x20000001a`
(a corrupted spanSet spine pointer) and Go heap corruption. Give-up must never
dispatch: deliver SIGSEGV/SIGILL to the thread (or refault) instead.

## Fix sketch (bug 1)

1. Snapshot `ESR_EL1` + `FAR_EL1` in the **vector asm at exception entry**,
   while PSTATE.I is still masked from the trap, into the `UserTrapFrame`; make
   the whole handler chain (SVC arm, both fault arms, the JIT-replay and
   VERIFY_SVC guards) consume the frame copies. Audit EVERY late `mrs esr_el1` /
   `mrs far_el1` in `src/exceptions.rs` (`grep -n "mrs.*esr\|mrs.*far"`).
2. Same audit for the EL1-sync and IRQ paths (any syndrome read after a
   yield/spin/enable-IRQ point).
3. Replace the give-up dispatch with signal delivery.
4. Keep a `SPURIOUS-SVC` counter; assert it is 0 in the boot self-tests and
   after acceptance stress runs. (Per `feedback_kernel_tests`: kernel changes
   need a `src/process_tests.rs` self-test — the race itself is hard to force,
   but the counter assertion + a Go-stress acceptance check covers regression.)

## Bug 2 — BKL wedge at teardown (SMP=4)

Onset is **exactly the parent's duration deadline** (T33.1x in the log): the
parent wakes, SIGTERMs 3 multithreaded Go children (cross-core
`pend_signal_for_thread` + `interrupt_thread` + group teardown), and the box
wedges: core 2 owns the BKL forever, all peers spin (`owner=2` constant), **zero
`[BKL] RECOVERED`** (so NOT the ticket leak — the owner is genuinely stuck in
EL1), zero `[WATCHDOG]` (no long preemption-disable), workload frozen.

May be downstream of bug-1 corruption (a Go child with a corrupted heap dying
mid-teardown) — so fix bug 1 first, then re-run the repro 5×. If the wedge
persists:

- Attach lldb BEFORE the wedge: `INSTANCE=1 GDB=1 SMP=4 overlays/devbox/run-smoltcp.sh`
  (gdbstub :1235; see `akuma_lldb_gdbstub_debugging` memory / `debug-smp.md`),
  let it wedge, halt, and backtrace **core 2** — the stuck owner is directly
  observable. Dump `KERNEL_LOCK` (owner/next_ticket/now_serving) and check for a
  cooperative wait loop or inner-lock spin under the BKL in the
  signal/teardown path (the M5a `PreemptGuard` discipline may be missing on a
  signal-delivery or exit-path lock).

## Related single-core anomaly (bug 3, low priority but diagnostic gold)

`forktest_smp1_fixed.log` has ONE `[BKL] stuck: owner=0 waiter=1` +
`[BKL] RECOVERED (advanced-lost) by core 1` — at **SMP=1**. The ticket leak
reproduces with a single core, so the standing lead suspect ("thread migrating
cores mid-EL1-hold") is wrong or incomplete. A single-core repro is a much
easier root-cause target for the open BKL accounting leak.

## Using forktest stress modes for lock-granularity (M5) work

The child's flags isolate subsystems, so each mode is a per-subsystem BKL
contention probe (combine with the BKL-hold profiler,
`akuma_exec::sync::set_profiling` + `WAIT_BY_HOLDER`, and
`sync::contention_spins` for A/B):

| Flag | Subsystem exercised | Lock-split work it measures |
|------|--------------------|-----------------------------|
| `-mmap_test -mmap_alloc_mb=N` | mmap/munmap + demand paging + PMM | `as_lock` (M5b), PMM/heap split |
| `-file_io` | VFS + block I/O under BKL | VFS/block split, Pass-B BKL-drop (M5b-4a) |
| `-goroutine_stress` | futex, timers, SIGURG preemption signals, scheduler | run-queue split (M5c), futex/signal locking |
| `-combined_stress` | all of the above | whole-system regression gate |
| `-use_c_child` | same stresses, pure C musl child | kernel-vs-Go-runtime disambiguation |

Suggested loop for each M5 split: profile with the matching single mode
(baseline `WAIT_BY_HOLDER` + `contention_spins`) → implement split → re-profile
same mode → gate with `-combined_stress` at SMP=2 and SMP=4. `quick_forktest.py`
(repo root) already automates boot+run+log-scrape; extend it to sweep modes.

## Success Criteria

- `grep -c SPURIOUS-SVC` == 0 on SMP=2 and SMP=4 combined_stress runs.
- SMP=4 `-num_children=3 -duration=15s -combined_stress` EXIT=0, 5/5 runs, no
  wedge at the deadline, 0 WILD-DA / 0 WATCHDOG / (transient RECOVERED only if
  bug 3 still open).
- `sshd_crash_hunt.py` (SMP=4, 3 boots) stays clean — no regression from the
  ESR-capture change.
- Boot self-tests green at SMP=1/2/4; 125+ akuma-exec host tests; clippy clean.

## Files

- `src/exceptions.rs` — vector asm + `rust_sync_el0_handler{,_inner}`, fault
  arms, VERIFY_SVC guard (~line 2100), `entry_esr` capture (~line 992).
- `crates/akuma-exec/src/sync.rs` — KernelLock, ticket self-heal, profiler.
- `crates/akuma-exec/src/process/signal.rs` + `src/syscall/proc.rs` (tkill/kill
  paths), teardown in `process/lifecycle.rs` — bug 2 territory.
- `src/config.rs` — `FUTEX_DBG_ENABLED`, `DEADLOCK_THREAD_DUMP_ENABLED` debug
  levers (flip for instrumented runs; revert before handing back).
- `docs/runbooks/debug-smp-fork-corruption.md` — prior dossier (mixed-EL family,
  LifecycleGuard, POISON tripwires); the phantom-SVC mechanism may explain
  earlier "context corruption" sightings too.
