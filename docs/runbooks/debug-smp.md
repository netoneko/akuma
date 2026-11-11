# Debug SMP (shared-kernel multiprocessing)

Symptom-driven debugging for **real (shared-kernel) SMP** — `cfg(kernel_bkl_smp)`,
the `smp-shared` feature, in the default feature set since 2026-08-10. For architecture
see [`../reference/subsystems/smp-shared.md`](../reference/subsystems/smp-shared.md); for
the running dev log see [`../archive/SMP_SHARED.md`](../archive/SMP_SHARED.md).

> **Stability: C (active development).** M0–M4 + M5a/M5b/M5c done. The BKL is now a **fair
> FIFO ticket lock**, which (with `idle_halt` on cooperative waits) fixed the former
> nondeterministic SMP=4 contention hang — the self-test suite now reaches the
> `test_mmap_file_oom` baseline reliably at SMP=4. Run devbox single-core unless you are
> specifically working on SMP.

## Mental model in 60 seconds

- **One Big Kernel Lock (BKL)** serializes all kernel (EL1) execution across cores. It is
  **owner-tracked, idempotent, binary**: "held by a core iff that core is in EL1",
  reconciled at every EL transition rather than balanced (`akuma_exec::sync::KernelLock`,
  driven via `akuma_exec::bkl`). Entry from EL0 acquires; `eret` to EL0 releases; a nested
  EL1 exception and its return leave it held. So there is exactly one release per
  excursion — no per-thread lock depth crosses a context switch.
- **The IRQ path holds the BKL across the whole excursion** (`rust_irq_handler_with_sp`),
  including the scheduler decision + context save, then **reconciles** the BKL to the EL of
  the frame it will `eret` into (release for EL0, keep/acquire for EL1) using the SPSR at
  `IRQ_FRAME_SPSR_OFFSET`. After a context switch that frame is the *incoming* thread's.
- **Userspace holds no BKL** — genuine parallelism. Faults' block I/O is dropped off the
  BKL (M5b Stage 4a); PTE edits are serialized by a per-address-space lock
  (`Process::as_lock`) so a fault and an `munmap`/`mmap`/`mprotect` on the same address
  space exclude correctly.
- **The shared run queue** is `POOL: Spinlock<ThreadPool>` (`threading/mod.rs`); each
  core's current thread is `TPIDRRO_EL0` (hardware, per-core).

## Build & run

```bash
cargo build --release
SMP=2 cargo run --release     # -smp 2
SMP=4 cargo run --release     # -smp 4
```

`SMP=N` (cargo_runner.sh) sets QEMU `-smp N`. Boot self-tests
(`crates/akuma-kernel-tests-aarch64/src/process_tests.rs::test_smp_shared_*`) run when tests are compiled in (no
`no-tests`). The devbox image is `--features no-tests`.

## Symptom → cause → fix

| Symptom / signature | Cause | Fix / next step |
|---|---|---|
| Boot freezes at `SMP>1` between `[SmolNet] Initialized successfully` and `[Net] virtio-net IRQ: slot 0 -> INTID 48`, with `[BKL] stuck: owner=1 waiter=2/3/4 (aff0+1)` repeating — i.e. **core 0** holds the BKL and every secondary is behind it | `register_handler` called `gic::enable_irq()` **while holding `IRQ_HANDLERS`**, a non-reentrant spinlock that `dispatch_irq` takes from the interrupt vector. If the line delivered on that core before the guard dropped, the core spun against itself — with the BKL still held. It also held that lock across up to 49 `Vec::push` heap allocations, widening the window | **FIXED 2026-08-24**: fixed `[Option<IrqHandler>; 256]` table (no allocation), publish under `with_irqs_disabled`, and `enable_irq` moved **outside** the lock — the half that actually closes it. [`../archive/IRQ_HANDLER_TABLE_DEADLOCK.md`](../archive/IRQ_HANDLER_TABLE_DEADLOCK.md) |
| SMP=4 `[BKL] stuck: owner=N waiter=M tag=511` **storm** (idle boot ~13 s in, or under rustc/fork load), often preceded by an EL1 crash dump with `ELR=0x8` or ELR in kernel *data*, `SP` outside the reported thread's stack, DAIF fully masked | **FIXED (2026-08-02): cross-core stack-sharing races** — a peer core picked up a thread that was still executing (or mid-switch) on another core's stack: (a) the switch-out tail after POOL release but before the vector asm's `mov sp, x0`; (b) wake-before-switch-out (`schedule_blocking` stores WAITING while still running; a peer wake-pass/futex-wake made it READY and a peer resumed it from stale `ctx.sp`). The storm is the corpse: the wild branch happens BKL-held and the core never releases. Same root cause as `BKL_RUSTC_SCALING_BASELINE.md` §5.1's ERET-to-EL0 with a kernel register file. | Fixed by the **ON_CPU scheduler gate** (pickers and the slot recycler skip threads whose gate is set; cleared by `rust_switch_finished` from the vector asm after `mov sp`). Full write-up: [`../archive/SMP_SHARED_ONCPU_GATE.md`](../archive/SMP_SHARED_ONCPU_GATE.md). If a storm reappears, check the `[SVC POISON]`/`[IRQ POISON]`/`[SGI-S STACK]` tripwires and the EL1 dump's `x30` line first. |
| `SMP=4` boot suite stops emitting after ~23 tests (QEMU alive, serial log frozen), ~81 `[BKL] stuck owner=1` lines then silence, freeze right after `smp_shared_cooperative_wait PASSED` — while `SMP=1` reports a clean 286/0 | **Someone changed `blocking_relax`.** Its leading `yield_now` is what lets a spawn/exec/reap waiter run a READY peer on its own core; halting instead leaves the peer unscheduled and the wait never resolves — the original socket-recv / `exec_with_io_cwd` wedge. Reproduced deterministically 2026-08-20 by dropping the yield kernel-wide. | Put the yield back in `blocking_relax`. If the motive was network throughput, use `blocking_relax_net` (no yield) wired into `NetRuntime::blocking_relax` **only** — that keeps +30 % req/s and passes 294/0. **Acceptance signal is the line `smp_shared_blocking_wait_peer_progress PASSED`, not the pass count, and NOT the `[BKL] stuck` count — that is ~85 on healthy and wedged kernels alike.** [`../archive/BLOCKING_RELAX_YIELD_SMP4_REGRESSION.md`](../archive/BLOCKING_RELAX_YIELD_SMP4_REGRESSION.md) |
| `[BKL] stuck: owner=N waiter=M` **flood that never stops**, no forward progress | A core is wedged in EL1 holding the BKL (real deadlock) — OR the BSP panicked while holding it (see the panic dump just above the flood). | If preceded by a panic dump, it's that panic — fix the panic, the flood is an artifact. Else attach gdb (below) and find what `owner`'s core is spinning on. |
| A few dozen **transient** `[BKL] stuck` during a spawn-heavy / FS test, then progress | Coarse-BKL contention — a core held the BKL >10 M spins (~10 ms) while peers waited. Expected today. | Profile it (below). The dominant holder is usually the scheduler. Not a bug. |
| `SMP=4` works for several exchanges then **wedges under sustained fork/exec + network load** (e.g. meow hangs on ~turn 4), flood is `owner=0` (unowned!) and never stops | **ROOT-CAUSED (2026-07-20): a `sched_bklfree_el0` ticket leak.** `owner=0` in the stuck line means the lock is *unowned* (`owner` stores `core_id+1`) yet waiters spin — the fair `KernelLock`'s `next_ticket` ran ahead of `now_serving` with no owner to advance it. The BKL-free EL0-preempt path is the only one that `reconcile`-acquires without a paired `enter_kernel`, so it can take a ticket with no matching release. | **FIXED PROPERLY 2026-08-08 — the ticket accounting itself.** (History: first mitigated by reverting `sched_bklfree_el0` to default-OFF; A/B then was flag-ON wedges in seconds, flag-OFF 13/13 clean. The flag is **default-ON again today** — `SCHED_BKLFREE_EL0_ENABLED = true`, `src/smp_shared.rs:435` — so that mitigation is no longer what protects you.) The leak is that a barge (`acquire_no_ticket`) consumed a live waiter's serving slot, the waiter re-ticketed, and the abandoned allocation was never matched by an ownership episode. Barges then left the FIFO untouched; since 2026-11-11 there is no barge at all (the path that used it went with the SGI scheduler). A/B at SMP=4, 30x `bssfork 20 3 1`: 140 stuck / 25 `advanced-lost` → **0 / 0**. Full invariant + how to read the diagnostics: [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) -> "The FIFO ticket invariant". To confirm an imbalance live: `lldb`, `memory read --size 4 --count 4 &KERNEL_LOCK` → `next_ticket` > `now_serving` with `owner==0`. |
| SMP=4 **nondeterministically wedges** in a spawn/timing-heavy test (`parallel_processes`, "Mixed cooperative") — a re-run passes | **FIXED (2026-07-20).** Was a cooperative wait-loop (`yield_now` only) holding the BKL while a child stranded RUNNING on a BKL-blocked peer, the unfair test-and-set then livelocking recovery. | Fixed by `idle_halt` on the wait loops (drop the BKL) + the fair FIFO ticket BKL. If a *new* wedge appears, check for a yield-only wait holding the BKL (see [`M5C_BKLFREE_EL0_SCHEDULER_DEADLOCK.md`](../archive/M5C_BKLFREE_EL0_SCHEDULER_DEADLOCK.md)). |
| `owner=N waiter=N` (same core) self-deadlock | A non-idempotent re-acquire, or holding a spinlock then taking the BKL and being re-entered. | `KernelLock::acquire` handles nested `enter_kernel` from a timer IRQ two ways: a reentrant fast path (`owner==me` → return, no ticket) and **masking local IRQs across the ticket wait** so a nested exception can't take a second ticket. If you added a lock that nests with the BKL, check ordering: `BKL > as_lock > {PMM, page tables, block, ...}`. |
| `[SGI] POOL contended, skipped N ticks` | The scheduler SGI used `POOL.try_lock()` and the current thread already held POOL (mid-op). One preemption tick skipped — harmless; the next tick retries. | If it climbs without bound, don't assume a leaked `POOL` guard: [`scheduler.md`](../reference/subsystems/scheduler.md#the-observed-wedge-open) documents a reproducible never-stops case under heavy fork/thread-churn load where every ordinary holder is bounded and none leaks — instead a live holder is blocked waiting on a thread that can only be scheduled back in by the very SGI its `POOL` hold is starving (a circular wait through the scheduler itself). Identify the actual holder before concluding anything leaked. |
| Boots at SMP=1 but hangs/deadlocks at SMP≥2 in a subsystem | That subsystem isn't SMP-safe: an inner spinlock (`NETWORK`, `SOCKET_TABLE`, block, VFS) is held across a context switch, so under the BKL a peer spins on it while holding the BKL. | Wrap the critical section in a `PreemptGuard` (akuma-net pattern) so the inner lock is never held across a switch; or fold it under the BKL/`as_lock` ordering. |
| Text interleaves per-char on the console (`[SM[PS-sMP...`) | Multiple cores writing the UART with no cross-core console lock. | Cosmetic. Use `safe_print!` (heap-free, secondary-safe). |
| `forktest_parent` hangs at "Launching child 0", kernel alive (0 `[BKL] stuck`), parent stuck in a `futex` | **Launcher-specific, NOT a kernel-commit regression and NOT the ticket leak.** The *same* smp-shared kernel runs forktest cleanly when launched via the **in-kernel ssh server**, and hangs when launched via the **userspace `/bin/sshd`** (devbox-smoltcp). Controlled at SMP=1, flag off: main+in-kernel-ssh ✅, branch(`--features smp-shared`)+in-kernel-ssh ✅, branch devbox-smoltcp(userspace-sshd) ❌. Parent's Go runtime parks in a futex before its own duration deadline; children close pipes with `write_count=0`. | Debug the userspace fork chain `sshd → shell → forktest_parent → forktest_child`, not the BKL. Suspects: Go-runtime futex/timer/signal setup or pipe-`EPOLLHUP` after a deep fork. NOTE the earlier "regression vs main" here was **confounded** (main tested via in-kernel ssh, branch via userspace sshd) — a kernel-commit bisect will NOT find it (HEAD passes under in-kernel ssh). |
| Forked children (`/bin/sshd`, `/bin/busybox`, shells) *and already-running procs* `SIGSEGV` under SMP=4; `ssh` sees "connection reset / kex_exchange_identification" but kernel heartbeats continue | **Memory corruption of `Process` / saved-context / page-table state during the high-concurrency BRINGUP window** (userspace fault, kernel survives). **Heterogeneous** across boots (⇒ corruption): `x0..x19` zeroed / user-PC = kernel addr (resolves to `rust_sync_el0_handler_inner+0x0`) / null-write in musl startup / `[DA-MISS] ppid=0 … 0 mmap_regions`. **NOT the BKL-drops** (see the dedicated dossier — with both drops off EL1 is fully BKL-serialized and it still crashes ⇒ the exposure is *not-atomic-across-preemption* lifecycle ops and/or genuinely-parallel EL0). **Also ruled out:** CoW/eviction/refcount (refcount-defended), thread-slot-reuse zeroing (CAS-guarded), ext2/block read (spinlocked). | **Full dossier: [`../archive/SMP_FORK_EXEC_CORRUPTION_FIX.md`](../archive/SMP_FORK_EXEC_CORRUPTION_FIX.md)** — symptom, repro (`scratchpad/sshd_crash_hunt.py`), the decisive drops-off experiment, the ruled-out list with file:line, and the rank-ordered hypotheses (start: `replace_image` not atomic across preemption; `THREAD_CONTEXTS` aliasing). |

## Profiling BKL contention (who holds the lock?)

`crates/akuma-exec/src/sync.rs` has a **BKL-hold profiler**, gated off by default
(`set_profiling(true/false)` — it false-shares a per-core tag line on every kernel entry,
which perturbs timing, so keep it off outside a measurement window):

- `set_holder_tag(core, tag)` is called at each kernel entry (syscall number / `HOLD_TAG_FAULT`
  / `HOLD_TAG_IRQ`) to record what a core is doing while it holds the BKL.
- A waiter samples the blocking owner's tag on first contention and adds its spin count to
  `WAIT_BY_HOLDER[tag]` — so `wait_by_holder(tag)` tells you which excursions made peers wait.
- `contention_spins()` / `reset_contention_spins()` give the total cross-core BKL wait (a
  wait-time proxy) for A/B measurement.

`test_smp_shared_fault_parallelism` (process_tests.rs) is the template: it toggles an
optimization, runs a workload, and dumps the top holders. **Finding (2026-07):** under a
multi-process (spawn) workload the **IRQ/scheduler path holds ~70 %** of contended BKL
time, faults ~20 %, syscalls the rest — so the scheduler is the dominant contention source.

To A/B a change safely, measure at **SMP=2** (fewer cores → less timing noise), enabling the
profiler only around the window.

### Runtime toggles (A/B / debugging)

- `smp_shared::set_fault_bkl_drop_enabled(bool)` — the M5b Stage 4a optimization (drop the
  BKL around a file-fault's block I/O). Default **on**.
- `smp_shared::set_exec_bkl_drop_enabled(bool)` — the M5c hold-shortening optimization (drop
  the BKL around execve's ELF reads: the main-binary read in `do_execve` **and** the
  dynamic-interpreter read in the ELF loader, the latter via the `ExecRuntime.exec_bkl_drop_enabled`
  hook). Default **on**. Safe: each read runs on a not-yet-installed image/AS and uses only
  VFS/ext2/`blk` + the self-locked heap allocator, none BKL-protected — the same profile as the
  fault drop. This is the "shorten BKL holds" lever (execve ELF-load was the dominant coarse-BKL
  hold). A/B-measured by the `smp_shared_exec_parallelism` self-test (SMP=2): busybox exec storm,
  ~26–63% fewer BKL spins with the drop ON across boots.
- `sync::set_profiling(bool)` — the BKL-hold profiler. Default **off**.

## Live debugging with gdb/lldb

The host has no `gdb`; attach `lldb` to QEMU's gdbstub. See
[`../../docs/`](../reference/subsystems/smp-shared.md) and the memory note on
lldb+gdbstub. Recipe:

```bash
INSTANCE=1 GDB=1 SMP=2 cargo run --release   # gdbstub on :1235
# in another shell:
lldb target/aarch64-unknown-none/release-smp-shared/akuma \
  -o "gdb-remote localhost:1235" -o "thread backtrace all"
```

For the devbox image add `DISK=devbox.img` and `--features no-tests`.
`thread backtrace all` shows every core; the BKL `owner` in a `[BKL] stuck` line is
`aff0 + 1`, so `owner=1` is core 0 (the BSP).

## Waiting for boot / SSH (never block on the QEMU job)

QEMU runs forever. Poll the log; never `job_output --wait` the QEMU process:

```bash
until grep -q "SSH Server\] Listening" run.log 2>/dev/null; do sleep 2; done
```

The self-test suite halts on a failed threading test
(`!!! THREADING TESTS FAILED - HALTING !!!`, an intentional semihosting `hlt`, *not* a
crash). `parallel_processes` timing out here used to be the SMP=4 race; that is fixed (fair
BKL + idle_halt) — if it recurs, treat it as a fresh regression, not "re-run and hope".

## Gotchas

**`[BKL] dropped window preserved across IRQ xN` is NOT a spin signature.**
`note_preserved_window` (`crates/akuma-exec/src/bkl.rs`) logs only when
`n.is_power_of_two()` — **the doubling is the sampler, not the phenomenon**.
Because the gap between lines doubles, it is disproportionately often the last
line in any log that stops for an unrelated reason, which makes it look like a
freeze cause. It appears on perfectly healthy VMs: a 4-second devbox boot that
went on to serve SSH emitted `x128 … x1024`. Read it as a counter. The real
`[BKL]` signature is `[BKL] stuck: owner=N`.

**`tag=511` in `[BKL] stuck` is always meaningless** — the holder-tag profiler
is off by default, so it reads 511 regardless. Read `owner=`, and remember
`(aff0+1)` means the numbers are core+1, so `owner=1` is **core 0**.

**Tell an idle hang from a spin from the host**, before theorising:
`ps -o %cpu -p $(pgrep -f qemu-system-aarch64)`. ~4 % is an idle guest waiting
for a wake that never came; ~100 %/core is a real spin. The two have completely
different causes and the console log looks identical.

**A console line is not atomic at SMP>1** (the console lock is currently off),
so another core can split one mid-write. A real boot produced
`[herd] Started [syscall] socket(type=TCP) = fd 3` / `sshd (pid= 2)` — i.e.
`Started sshd` cut in half — and a health check grepping for that marker
reported a **false failure on a working VM**. `grep -ac PASSED` can be split the
same way. Probe the service (an actual SSH round-trip) rather than grepping,
where you can.


- **Console garble** makes `grep` miss lines. Sanitize first:
  `LC_ALL=C tr -d '\r' < run.log | LC_ALL=C tr -c '[:print:]\n' ' '`.
- **`as_lock` must never be held across alloc / block I/O / a context switch** — the PMM
  OOM/reclaim path can re-enter it. Hold it only for short PTE-edit windows (IRQs off).
  Allocate before, free after.
- The `size` / `extreme` builds compile **none** of this — every
  `bkl`/`as_lock`/profiler entry is a zero-cost no-op unless `cfg(kernel_bkl_smp)`.
- All SMP work is gated; if a change regresses a non-SMP build, you broke a `cfg` gate.
