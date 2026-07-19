# Debug SMP (shared-kernel multiprocessing)

Symptom-driven debugging for **real (shared-kernel) SMP** — `cfg(kernel_smp_shared)`,
the `smp-shared` feature on the `release-smp-shared` profile. For architecture see
[`../reference/subsystems/smp-shared.md`](../reference/subsystems/smp-shared.md); for the
running dev log see [`../archive/SMP_SHARED.md`](../archive/SMP_SHARED.md). This is the
*inverse* of the share-nothing multikernel ([`../reference/subsystems/smp.md`](../reference/subsystems/smp.md),
`cfg(kernel_smp)`); the two are mutually exclusive.

> **Stability: C (active development).** M0–M4 + M5a/M5b done; the SMP=4 path has a
> known nondeterministic contention race (below). Run devbox single-core unless you are
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
cargo build --profile release-smp-shared --features smp-shared
SMP=2 cargo run --profile release-smp-shared --features smp-shared     # -smp 2
SMP=4 cargo run --profile release-smp-shared --features smp-shared     # -smp 4
```

`SMP=N` (cargo_runner.sh) sets QEMU `-smp N`. Boot self-tests
(`src/process_tests.rs::test_smp_shared_*`) run when tests are compiled in (no
`no-tests`). The devbox image is `--features devbox-smoltcp` (adds `no-tests`).

## Symptom → cause → fix

| Symptom / signature | Cause | Fix / next step |
|---|---|---|
| `[BKL] stuck: owner=N waiter=M` **flood that never stops**, no forward progress | A core is wedged in EL1 holding the BKL (real deadlock) — OR the BSP panicked while holding it (see the panic dump just above the flood). | If preceded by a panic dump, it's that panic — fix the panic, the flood is an artifact. Else attach gdb (below) and find what `owner`'s core is spinning on. |
| A few dozen **transient** `[BKL] stuck` during a spawn-heavy / FS test, then progress | Coarse-BKL contention — a core held the BKL >10 M spins (~10 ms) while peers waited. Expected today. | Profile it (below). The dominant holder is usually the scheduler. Not a bug. |
| SMP=4 **nondeterministically wedges** in a spawn/timing-heavy test (`parallel_processes`, "Mixed cooperative") — a re-run passes | The known nondeterministic SMP=4 coarse-BKL contention race. Independent of the fault path (reproduces with fault opts disabled). | Re-run; use SMP=2 for clean measurement. Real fix is fine-graining the scheduler off the BKL (M5c). |
| `owner=N waiter=N` (same core) self-deadlock | A non-idempotent re-acquire, or holding a spinlock then taking the BKL and being re-entered. | The BKL re-checks `owner==me` every spin iteration (`KernelLock::acquire`) to survive nested `enter_kernel` from a timer IRQ. If you added a lock that nests with the BKL, check ordering: `BKL > as_lock > {PMM, page tables, block, ...}`. |
| `[SGI] POOL contended, skipped N ticks` | The scheduler SGI used `POOL.try_lock()` and the current thread already held POOL (mid-op). One preemption tick skipped — harmless; the next tick retries. | Only worry if it never stops (a leaked POOL guard). |
| Boots at SMP=1 but hangs/deadlocks at SMP≥2 in a subsystem | That subsystem isn't SMP-safe: an inner spinlock (`NETWORK`, `SOCKET_TABLE`, block, VFS) is held across a context switch, so under the BKL a peer spins on it while holding the BKL. | Wrap the critical section in a `PreemptGuard` (akuma-net pattern) so the inner lock is never held across a switch; or fold it under the BKL/`as_lock` ordering. |
| Text interleaves per-char on the console (`[SM[PS-sMP...`) | Multiple cores writing the UART with no cross-core console lock. | Cosmetic. Use `safe_print!` (heap-free, secondary-safe). |

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

To A/B a change safely, measure at **SMP=2** (contention-clean; SMP=4's nondeterministic
race adds noise), enabling the profiler only around the window.

### Runtime toggles (A/B / debugging)

- `smp_shared::set_fault_bkl_drop_enabled(bool)` — the M5b Stage 4a optimization (drop the
  BKL around a file-fault's block I/O). Default **on**.
- `smp_shared::set_sched_bklfree_el0_enabled(bool)` — the M5c step-2 optimization (run the
  scheduler SGI BKL-free when it preempted EL0). Default **off**: correct at SMP=2 but hangs
  a process at SMP≥4 (`parallel_processes` "P1 done: false"), under investigation. The M5c
  step-1 foundation (POOL covers the whole context switch) is always active regardless.
- `sync::set_profiling(bool)` — the BKL-hold profiler. Default **off**.

## Live debugging with gdb/lldb

The host has no `gdb`; attach `lldb` to QEMU's gdbstub. See
[`../../docs/`](../reference/subsystems/smp-shared.md) and the memory note on
lldb+gdbstub. Recipe:

```bash
INSTANCE=1 GDB=1 SMP=2 cargo run --profile release-smp-shared --features smp-shared   # gdbstub on :1235
# in another shell:
lldb target/aarch64-unknown-none/release-smp-shared/akuma \
  -o "gdb-remote localhost:1235" -o "thread backtrace all"
```

For the devbox image add `DISK=devbox.img` and `--features devbox-smoltcp,no-tests`.
`thread backtrace all` shows every core; the BKL `owner` in a `[BKL] stuck` line is
`aff0 + 1`, so `owner=1` is core 0 (the BSP).

## Waiting for boot / SSH (never block on the QEMU job)

QEMU runs forever. Poll the log; never `job_output --wait` the QEMU process:

```bash
until grep -q "SSH Server\] Listening" run.log 2>/dev/null; do sleep 2; done
```

The self-test suite halts on a failed threading test
(`!!! THREADING TESTS FAILED - HALTING !!!`, an intentional semihosting `hlt`, *not* a
crash). `parallel_processes` failing there is usually the SMP=4 race — re-run.

## Gotchas

- **Console garble** makes `grep` miss lines. Sanitize first:
  `LC_ALL=C tr -d '\r' < run.log | LC_ALL=C tr -c '[:print:]\n' ' '`.
- **`as_lock` must never be held across alloc / block I/O / a context switch** — the PMM
  OOM/reclaim path can re-enter it. Hold it only for short PTE-edit windows (IRQs off).
  Allocate before, free after.
- The default / size / extreme / multikernel builds compile **none** of this — every
  `bkl`/`as_lock`/profiler entry is a zero-cost no-op unless `cfg(kernel_smp_shared)`.
- All SMP work is gated; if a change regresses a non-SMP build, you broke a `cfg` gate.
