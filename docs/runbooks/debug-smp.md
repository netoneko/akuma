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
| SMP=4 **nondeterministically wedges** in a spawn/timing-heavy test (`parallel_processes`, "Mixed cooperative") — a re-run passes | The known nondeterministic SMP=4 coarse-BKL contention race. Independent of the fault path (reproduces with fault opts disabled). | Re-run; use SMP=2 for clean measurement. Real fix is shortening BKL hold times (M5b per-AS fault lock; split fork/exec/ELF-load off the BKL) — **not** the BKL-free scheduler, which makes this worse (see "M5c step-2" below). |
| `[BKL] stuck: owner=1` flood, workload frozen, **only** with `sched_bklfree_el0` ON | The M5c step-2 **cross-core circular deadlock** (re-root-caused 2026-07-20 with lldb — the earlier "monopoly on an unfair lock" note was wrong; it's a hard hang, not a re-grab race). A BKL-free secondary claims a thread `RUNNING` without the BKL while the BSP holds the BKL in a cooperative yield-wait for it. | Keep step-2 gated off. See the dedicated section below. |
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
- `smp_shared::set_exec_bkl_drop_enabled(bool)` — the M5c hold-shortening optimization (drop
  the BKL around execve's ELF reads: the main-binary read in `do_execve` **and** the
  dynamic-interpreter read in the ELF loader, the latter via the `ExecRuntime.exec_bkl_drop_enabled`
  hook). Default **on**. Safe: each read runs on a not-yet-installed image/AS and uses only
  VFS/ext2/`blk` + the self-locked heap allocator, none BKL-protected — the same profile as the
  fault drop. This is the "shorten BKL holds" lever (execve ELF-load was the dominant coarse-BKL
  hold). A/B-measured by the `smp_shared_exec_parallelism` self-test (SMP=2): busybox exec storm,
  ~26–63% fewer BKL spins with the drop ON across boots.
- `smp_shared::set_sched_bklfree_el0_enabled(bool)` — the M5c step-2 optimization (run the
  scheduler SGI BKL-free when it preempted EL0). Default **off**: correct at SMP=2, but on
  the current coarse BKL it is counter-productive at SMP≥4 (**root-caused**, see below). The
  M5c step-1 foundation (POOL covers the whole context switch) is always active regardless.
- `sync::set_profiling(bool)` — the BKL-hold profiler. Default **off**.

## M5c step-2 (BKL-free EL0 scheduler): why it stays gated OFF

**Re-root-caused 2026-07-20 with lldb over the QEMU gdbstub.** The earlier (2026-07-19)
"monopoly on an unfair lock" explanation was **wrong** — reasoned, never confirmed at the
instruction level. It is a **hard cross-core circular deadlock**, not a fairness/re-grab
problem. Enabling `sched_bklfree_el0` and booting `SMP=4` reproduces a `[BKL] stuck:
owner=1` flood (~3000–5000 events, all `owner=1` = the BSP, vs. ~102 toggle-off) with the
workload permanently wedged. To reproduce: add
`smp_shared::set_sched_bklfree_el0_enabled(true)` right after `bringup_secondaries()` in
`main.rs` (do not commit), build, `SMP=4 GDB=1 INSTANCE=1 cargo run …`, grep the log for
`BKL] stuck`, then attach lldb on `:1235`.

> **Watch out — two overlapping symptoms.** The *terminal* state everyone sampled before is
> a red herring: it is the pre-existing `test_mmap_file_oom` **panic** → the panic's
> semihosting `HLT #0xF000` (`0xd45e0000`) → `EC=0x0` → `rust_sync_el1_handler`'s terminal
> `loop{wfe}` on core 0 *holding the BKL* (per the table row above: "if preceded by a panic
> dump, the flood is an artifact"). Sample the **pre-panic** flood (`grep -c PANIC` must be
> `0`) to see the real step-2 bug.

**Mechanism (confirmed by live register/memory state at the pre-panic wedge).**

1. The BSP (core 0) holds the BKL and is spinning in `exec_with_io_cwd`'s cooperative
   wait-loop — `while !channel.has_exited() && !is_thread_terminated(tid) { yield_now() }` —
   waiting for the EL0 child it spawned (backtrace: `run_all_tests → exec_with_io →
   exec_with_io_cwd → yield_now → …irq_handler → sgi_scheduler_handler_with_sp`). The loop
   runs entirely in EL1, so the BKL is never reconciled-to-EL0-released; the BSP holds it
   throughout.
2. The child thread is state **`RUNNING`**, and it is the current thread (`TPIDRRO_EL0`) of a
   **secondary** — but that secondary is frozen in the inlined `KernelLock::acquire` spin
   inside `enter_kernel` (`rust_irq_handler_with_sp`, the `ldar/cmp/b.eq` loop). It took a
   syscall/device IRQ while running the child and now waits on the BKL the BSP holds.
3. The BSP's `ThreadPool::schedule_indices` only ever selects a **READY, non-idle** thread.
   The child is `RUNNING` (skipped); the only READY thread is a per-core idle
   (`IDLE_SLOT_FOR_CORE`, skipped by `IS_IDLE_THREAD`). So the scan returns `None`, and the
   BSP spins its yield loop forever holding the BKL. **Circular wait:** BSP holds BKL & waits
   for child → child is RUNNING on a secondary → secondary needs the BKL → BSP holds it.

**Why the toggle is the trigger.** The BKL-free EL0-preempt path lets a secondary **claim a
READY thread and mark it `RUNNING` without acquiring the BKL**, while the BSP holds the BKL
in a cooperative wait for that thread. Once the child is `RUNNING` on a secondary the BSP
won't migrate it (it moves only READY threads) and the secondary freezes the instant the
child needs EL1. With the toggle **off** a secondary can only claim a thread by first taking
the BKL — impossible while the BSP holds it — so the child stays **READY** and the **BSP
itself** runs it, reconciling to EL0 and releasing the BKL. No deadlock. (The old note's
"per-tick circulation" is really "secondaries can't steal a thread out from under a
BKL-holding cooperative waiter.")

**The fix is TWO parts, both required** (measured at SMP=4 with the 40-iteration
exec-and-wait stress `test_smp_shared_cooperative_wait`, which deadlocks ~100% with step-2
on and no fix):

1. **A kernel thread must not hold the BKL across a cooperative wait-loop.** Drop +
   re-acquire the BKL around the `yield_now` wait — done for `exec_with_io_cwd` via
   `idle_halt` (`crates/akuma-exec/src/process/exec.rs`). This alone cuts the hang from
   ~100% to a **~25% residual**.
2. **A fair / queued BKL.** The residual ~25% is a *livelock*: after the waiter drops the
   BKL (idle_halt WFI), the unfair test-and-set lets the other spinning secondaries and the
   waiter's own next-tick re-grab starve the one secondary holding the BKL-free-stolen
   child, so it never gets the BKL to un-strand it. (Attaching gdb *hides* this — the stub's
   periodic all-core halts perturb the race enough to let it through; it reproduces without
   gdb.) A ticket/queued `KernelLock` removes it. **Not yet done — the remaining blocker.**

So do not treat step-2 as "just premature": it is a correctness deadlock, and only when
BOTH parts land does it become safe to flip on.

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
