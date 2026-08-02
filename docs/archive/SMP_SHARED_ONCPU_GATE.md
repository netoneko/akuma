# The ON_CPU gate: cross-core stack-sharing races behind the SMP=4 `[BKL] stuck` storms and §5.1 — 2026-08-02

**Status: FIXED (ON_CPU scheduler gate), verified by A/B under the rustc `big.rs`
hammer at SMP=4.** A residual, pre-existing rustc *hang* (futex-shaped, contained,
VM survives) remains open — see §6.

This closes the two headline SMP=4 instabilities as one bug:

- The `[BKL] stuck: owner=N waiter=M tag=511` storms (boot-time and under rustc
  load; `BKL_PHASE7F_OPTOUT_LIST.md` §9.1's "9855 lines in run 1, SSH unreachable").
- [`BKL_RUSTC_SCALING_BASELINE.md`](BKL_RUSTC_SCALING_BASELINE.md) §5.1's `big.rs`
  failure: an EL0 return with a **kernel** register context (`fault_pc` in kernel
  text, `x1` = own tid, `x3` = own pid), killed by rustc's own SIGSEGV handler
  faithfully restoring the corrupt context.

## 1. The two races

Both are in the scheduler (`crates/akuma-exec/src/threading/mod.rs`); thread STATE
alone (READY/WAITING/RUNNING) catches neither.

**1a. Switch-out tail.** `commit_switch` marks the outgoing thread READY, and POOL
is released when `sgi_scheduler_handler_with_sp` returns — but the switching core
keeps *executing on the outgoing thread's kernel stack* until the vector asm's
`mov sp, x0`: the Rust function epilogues, the BKL reconcile
(`reconcile_for_spsr*`), and the profiler tag bookkeeping in
`rust_irq_handler_with_sp` all run after POOL is gone, on the old stack. The M5c
design comment ("`pool` is held across the ENTIRE switch") was true of the
*decision + context save*, but the switch only completes in the asm. A peer core
that wins POOL in that window sees the outgoing thread READY, picks it, and
resumes it **onto the stack this core is still using**.

**1b. Wake-before-switch-out** (the frequent one). `schedule_blocking` stores
WAITING — often with an already-expired deadline; the devbox sshd does 400+
short nanosleeps/s — *while the thread is still running*, before its yield-SGI
fires. A peer's wake-pass (`schedule_indices`) or a cross-core
`mark_thread_ready`/futex wake flips it READY, and a peer scheduler picks it and
"resumes" it from its **stale `ctx.sp`** (the frame from its *previous*
switch-out) while it is still running on the original core. Two cores then
execute on one kernel stack.

## 2. Observed signatures, decoded

Frames collide in both directions, so the corruption wears several costumes:

- `[Exception] Sync from EL1: EC=0x0, ELR=0x8, SPSR=0x800003c5, Thread=0,
  SP=0x603f5400` — thread 0's core did a `ret` through a clobbered spill slot
  (`x30=0x8`), inside a DAIF-masked section, with SP in *another thread's* pool
  stack. "Thread=0" because the per-core current-thread register was already
  switched to the incoming tid; the SP was the outgoing thread's.
- Same shape with `ELR=x30=0x402e1ad8` (kernel **data**; instruction bytes
  `0x02010201`) — `ret` into a data address read from a clobbered frame.
- §5.1's EL0 variant: the victim's mid-syscall frames absorb the other core's
  scheduler locals (which are exactly tids/pids/kernel pointers); the unwind goes
  wild and the SVC epilogue erets to EL0 with kernel values in the register file.
- The `owner=N tag=511` storm is the *aftermath*: the wild branch happens with the
  BKL held (or corrupts the ticket state), the core never releases, and every
  peer's `enter_kernel` prints `[BKL] stuck` forever. The storm is the corpse,
  not the disease.

Boot-time reproduction needs nothing but `release-smp-shared
--features devbox-smoltcp,no-tests` at SMP=4: sshd's accept/nanosleep churn alone
fired it ~1-in-5 boots (~13 s in). rustc `big.rs` at conc=4 fired it nearly every
run on the pre-fix kernel.

## 3. The fix: a per-thread ON_CPU gate

`ON_CPU: [AtomicU8; MAX_THREADS]` — set while a thread is (or may still be)
executing on some core; a thread is **pickable only when its gate is clear**.

- `commit_switch` latches the gate for the incoming thread (and re-asserts the
  outgoing thread's), and records the outgoing tid in a per-core slot
  (`PER_CORE_OFFCPU`).
- The vector asm calls `rust_switch_finished` immediately **after** `mov sp, x0`
  (both `irq_el0_handler` and `irq_handler` in `src/exceptions.rs`) — only then is
  the outgoing thread's gate cleared (Release), because only then is the core off
  its stack. The clear also release-publishes the freshly saved `ctx.sp`.
- Every picker — round-robin scan, `PREEMPT_WAKE_TID` hint, network-boost, and the
  per-core idle fallback — skips gated threads. A skipped wake is only deferred to
  the next tick, never lost (state stays READY).
- `cleanup_terminated_internal` skips gated slots, so a stack cannot be freed
  under a core that is still mid-switch on it.
- Bringup latches: threading init (boot thread 0) and the per-core idle-slot claim
  set the gate for the thread each core starts life running.

Boot-suite self-test: `test_on_cpu_gate_lifecycle` (`src/process_tests.rs`).

## 4. Diagnostics added on the way (all kept)

- `[SVC POISON]` / `[IRQ POISON]` — the two previously-untripwired eret paths (the
  SVC epilogue restore and the IRQ epilogue, both outcomes) now check the frame
  they are about to restore: EL0-target ELR must NOT be in kernel text
  (`[0x4010_0000, 0x6000_0000)`), EL1-target ELR MUST be. **The kernel-text range
  matters**: the old `elr >= 0x4000_0000` predicate false-positives on user mmap
  VAs (rustc maps at `0x1_xxxx_xxxx`+) — observed 3452 bogus `[SGI-S POISON]`
  lines in one run. `[SGI-S POISON]` was re-predicated the same way.
- `[SGI-S STACK]` — at switch time, the frame about to be restored must lie within
  the incoming thread's own stack bounds (`pool.stacks[new_idx]`).
- The EL1 sync exception dump now prints `x0–x3, x29, x30` (the vector passes a
  pointer to its saved-register block). `x30` names the caller on a wild `blr`,
  and `x30 == ELR` fingerprints a wild `ret`. This is what cracked the case.

## 5. Verification (A/B, same disk, same SMP=4 QEMU config, `big_hammer` —
rounds of 4 concurrent `rustc -O big.rs` with artifact verification)

| | baseline (pre-fix) | ON_CPU fix |
|---|---|---|
| boot-time EL1 crash → owner=N storm | ~1-in-5 idle boots | not observed |
| hammer outcome | 1 clean round, then hard `owner=4` storm, SSH dead | no storms, no faults, VM stays fully alive across all runs |
| `[BKL] stuck` during hammer | 2224 and climbing (wedged) | 105 in one transient cluster, then 0 |
| kernel-text POISON tripwires | (wedged before meaningful) | **0 fires** |
| boot suite (single-core `--release`) | — | 241 PASSED, incl. new self-test |

Host: full-workspace `cargo test` 478 passed / 0 failed; clippy clean on
`release-smp-shared --features devbox-smoltcp,no-tests` and `--release`.

## 6. What remains: the contained rustc stall/hang

With the fix, `big.rs` conc=4 rounds still intermittently take ~4× the normal
wall-clock or fail with "artifact absent, rustc silent" — but the failure is now
*contained*, not corruption: the VM stays fully healthy (sshd serving, no
storms). One captured instance was a genuine in-guest freeze: rustc alive but
zero syscall progress for 4+ minutes while sshd sustained its normal ~650
syscalls/s (so NOT host contention), all threads parked in `futex` op 0x80/0x89
plus one sibling in syscall 93 `exit`, `{lto cgu.0}` never finishing. A later
traced instance (FUTEX_DBG build) completed after 239.5 s with no futex wait
longer than ~49 s — that one is confounded by concurrent host load (the boot log
shows `Time jump detected (host sleep/wake)`).

Evidence the mode is pre-existing and not introduced by the gate: the
2026-08-01 baseline already failed `big conc=4` at **SMP=1** both reps with the
same silent signature — no cross-core race is possible on one core.

**Harness trap that manufactures false "hangs": the driver's ssh channel dies at
~240 s** (`ServerAliveInterval=60` × default `CountMax=3` once the guest stops
answering keepalives under full CPU load) — a slow-but-alive compile then reads
as "artifact absent". Every ~240 s "failure" timestamp in this campaign matches
that constant. Drive long compiles with `nohup … &` + artifact polling (the
`llama-on-smp-shared-blockers` lesson), or crank keepalives way up.

Next chase (now cheap, since the VM survives): hammer with the host quiescent
and nohup-style rounds; when a compile exceeds ~5 min, inspect LIVE with
`FUTEX_DBG_ENABLED` + `DEADLOCK_THREAD_DUMP_ENABLED` (`src/config.rs`) — the
heartbeat dumps per-thread park sites with the futex uaddr/op in `a0/a1`.
Suspects, in order:

1. **`ppoll` lost wakeup (jobserver pipe)** — NEW, from the final traced capture:
   the stuck rustc's MAIN thread sits in `ppoll` (sc=73) while its LLVM workers
   futex-wait downstream; rustc's jobserver is a pipe+poll loop, so one missed
   pipe-readable event starves the whole pool, and the futex waits are
   *symptoms*. The 7b ppoll carve-out is exactly the conversion with a
   documented 1-in-2 flaky regimen failure
   ([`BKL_PHASE7B_PPOLL_CARVE_OUT.md`](BKL_PHASE7B_PPOLL_CARVE_OUT.md)).
   Supporting: traced WAIT→WOKE futex pairs all complete (longest legit ~49 s),
   i.e. the futex machinery itself looked healthy in the same run.
2. The tranche-3 BKL-free `futex` conversion's wake/exit interplay
   (`[exit93]`/`[cct-exit]` FUTEX_DBG instrumentation already exists from a
   prior chase) — rustcs were also seen lingering in teardown (one thread in
   syscall 93) long after their artifact was written.
3. `sys_exit`'s CLEARTID skip-write-on-unmapped path (wakes the joiner without
   zeroing the tid → the joiner re-waits forever).

Also observed in the hung guest, worth its own look: `busybox ps` SIGSEGVs
while the stalled rustcs exist (procfs iteration racing teardown?).

## Background

- [`BKL_RUSTC_SCALING_BASELINE.md`](BKL_RUSTC_SCALING_BASELINE.md) §5.1 — the
  failure that started this chase.
- [`CURRENT_TRAP_FRAME_STALE_ON_EXIT.md`](CURRENT_TRAP_FRAME_STALE_ON_EXIT.md) —
  the adjacent defect found (and fixed) on the way; its §4 correctly concluded it
  did not explain §5.1.
- [`SMP_SHARED.md`](SMP_SHARED.md) / `docs/runbooks/debug-smp.md` — earlier
  `owner=1` storm investigations (M5c-era), which saw the switch-out hazard for
  `ctx.sp` but not the still-running pickup.
- [`BKL_PHASE7F_OPTOUT_LIST.md`](BKL_PHASE7F_OPTOUT_LIST.md) §9 — the tranche-3
  futex conversion, prime suspect for §6.
