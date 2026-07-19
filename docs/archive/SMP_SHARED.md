# Real (shared-kernel) SMP — progress log

Running log of the real symmetric-multiprocessing effort: ONE shared kernel across
all cores (one page-table set, one PMM/heap, one run queue, real cross-core locking).
This is the **inverse** of the multikernel in `MULTIKERNEL.md` (one-kernel-per-core,
share-nothing). For the current-state design see
[`../reference/subsystems/smp-shared.md`](../reference/subsystems/smp-shared.md).

Everything is behind `cfg(kernel_smp_shared)` (the `smp-shared` feature, paired with
the `release-smp-shared` profile). `build.rs` makes `smp` and `smp-shared` mutually
exclusive. The default build compiles none of it.

Design decisions (user-approved plan): coexist with the multikernel as a new feature;
incremental milestones M0..M5; **Big-Kernel-Lock first**, fine-grained later; single
global run queue + lock; per-core current thread via `TPIDRRO_EL0`. Developed and
verified on the **devbox-smoltcp** image (native smoltcp stack, built-in ssh dropped),
which is now the default devbox — rump_server work is deferred.

---

## M0 — Cores online on the shared kernel ✅ (2026-07-19)

**Goal:** bring N cores up on the *shared* boot page tables, PMM, and heap — no
isolation, no partitions. Each secondary reports online and parks in `WFE`.

**Implementation** (`src/smp_shared.rs`, new; ~300 lines):
- Build wiring: `smp-shared` feature + `cfg(kernel_smp_shared)` (build.rs), the
  `release-smp-shared` profile (Cargo.toml), a 4 MB size branch in
  `scripts/cargo_runner.sh`.
- Topology probe (`probe_dtb`, called from `kernel_main` before heap init): parses
  `/cpus` + `/psci`, stashes MPIDRs indexed by `aff0 = mpidr & 0xff`. Compact copies
  of the multikernel's `read_mpidr`/`psci_call`/`resolve_dtb`/`psci_is_hvc` so
  `src/smp.rs` stays byte-for-byte untouched.
- Bringup (`bringup_secondaries`, after `gic::init`): PSCI `CPU_ON` each secondary at
  the new `secondary_entry_shared` trampoline, then a bounded spin until every one
  bumps the shared `ONLINE_COUNT`.
- Trampoline (`secondary_entry_shared`, `global_asm!` in `.text.boot`): mirrors
  `crate::smp::secondary_entry` + boot.rs MMU setup, but loads the BSP's **shared**
  boot `TTBR0`/`TTBR1` (`boot_ttbr0_addr`/`boot_ttbr1_addr`, published MMU-off by
  boot.rs) — never a restricted per-core table — and tail-calls
  `secondary_shared_start`. Own 16 KiB/core stacks in `.bss.smp_shared`.
- `secondary_shared_start(_ctx, core_idx)`: `ONLINE_COUNT.fetch_add(1)`, `safe_print!`
  "core N online", then `WFE` park.

**Key realizations:**
- The trampoline's shared-table load is exactly what real SMP wants — the multikernel
  only diverges into an isolated table *after* the trampoline, so no `smp.rs` change
  was needed, just a fork of the post-trampoline path.
- The boot page tables identity-map device space via the L1[0] 1 GiB block
  (0x0–0x40000000), so a secondary on the boot tables reaches the shared UART (and
  later its own GIC redistributor) directly — **no boot.rs device-map change needed
  for M0.** The planned "map all GICR frames" step is deferred to M2, when secondaries
  actually enable interrupts.
- A plain kernel `static` (e.g. `ONLINE_COUNT`) is genuinely shared cross-core in this
  build (not replicated) — the whole premise, and it works.

**Verification** (`release-smp-shared`, MEMORY=256M):
- `SMP=2`: `[SMP-shared] ✓ 1 secondary core(s) online (shared kernel)`;
  boot self-test `[Test] smp_shared_cores_online PASSED (1/1 ...)`.
- `SMP=4`: `✓ 3 secondary core(s) online`; `PASSED (3/3 ...)`.
- Boot self-test in `src/process_tests.rs::test_smp_shared_cores_online`, run FIRST in
  `run_all_tests` so it is observed even though an unrelated, pre-existing
  memory-pressure test (`test_mmap_file_oom`, process_tests.rs:4690) aborts the suite
  later at 256M — confirmed pre-existing: identical panic with `SMP=1`, no secondary
  running.

**Known M0 cosmetic:** multiple cores writing the shared UART concurrently interleave
characters (`[SMP[S-sMP-sharedhared]`), because there is no cross-core console lock
yet. Harmless; addressed when the console is serialized (M1/M2).

**Files:** `src/smp_shared.rs` (new), `src/main.rs` (module + probe/bringup calls),
`build.rs`, `Cargo.toml`, `scripts/cargo_runner.sh`, `src/process_tests.rs`.

---

## M1 — Big Kernel Lock: primitive + syscall-path wiring ✅ (2026-07-19)

**Goal:** introduce the BKL and take it on the hot syscall path, uncontended, without
regressing the BSP — the foundation M3's process-table safety relies on.

**The lock** (`crates/akuma-exec/src/sync.rs::KernelLock`): an **owner-tracked,
idempotent** spinlock, *not* a counted/recursive one. Invariant: **held by a core iff
that core is executing kernel code (EL1)**, reconciled at EL transitions rather than
balanced. Because there is exactly one EL1→EL0 return per kernel excursion, there is
exactly one release per excursion — so no per-thread lock depth has to travel across
context switches (the trap that killed Linux's BKL simplicity). Idempotent
acquire/release (re-acquiring what you hold, releasing what you don't — both no-ops)
makes it robust against the non-lexical acquire/release that context switches create.
6 host tests in sync.rs.

**Why idempotent/binary works here:** the boundary map found (a) no synchronous
context switch anywhere — every switch (yield/block/preempt) goes through the IRQ-
vector SGI path; (b) the `eret` target EL is set by the *incoming* thread's restored
SPSR, decoupled from which handler runs; (c) `sync_el0_handler` enables IRQs during the
syscall. A counted lock would need per-thread depth save/restore across every switch; a
binary "held iff in EL1" lock needs only reconciliation at EL crossings. The owner-CAS
is atomic regardless of local IRQ state, so the lock is **IRQ-state-agnostic** — it can
be driven from Rust, avoiding fragile edits to the exception assembly.

**Global module** (`crates/akuma-exec/src/bkl.rs`): the one `KERNEL_LOCK` +
`current_core_id()` (MPIDR aff0) + `enter_kernel`/`leave_kernel`/`reconcile_for_spsr`/
`held_by_current`. Every function is a **zero-cost no-op unless `cfg(kernel_smp_shared)`**
(the `smp-shared` feature, now forwarded to akuma-exec via its own build.rs, mirroring
`extreme`). Default / size / extreme / multikernel builds are byte-for-byte unaffected.

**Wiring (M1 scope = the syscall excursion only):**
- `src/exceptions.rs::rust_sync_el0_handler` split into a thin BKL wrapper
  (`enter_kernel` → inner → `leave_kernel`) around the renamed
  `rust_sync_el0_handler_inner`. A thread servicing an EL0 trap (syscall or fault) holds
  the BKL for the whole excursion, so its process-table / VFS / net / page-table access
  is serialized against other cores.
- `crates/akuma-exec/src/process/mod.rs::enter_user_mode` calls `leave_kernel` before
  its `eret` — initial launch / execve drop to EL0 without returning through the wrapper.

**Deferred to M2 (deliberately — needs cross-core contention to test):** the
IRQ/scheduler context-switch reconciliation (`reconcile_for_spsr` at the switch, using
the incoming thread's frame SPSR), idle-loop release-around-WFI, and kernel-thread BKL
handling. On the single active core of M0/M1 a syscall thread may still be preempted
mid-excursion; that is harmless here (idempotent ops, no contention) and becomes correct
once M2 adds reconciliation. `ThreadPool.current_idx` retirement also deferred to M2
(it's already a redundant mirror of `TPIDRRO_EL0`; low-value churn until the scheduler
changes).

**Verification** (`release-smp-shared`, SMP=2): BSP boots, the full boot suite runs
through the BKL wrapper (futex + process tests), `smp_shared_cores_online PASSED`, and
**zero `[BKL] stuck`** (no deadlock). Reaches the exact same pre-existing baseline
(`test_mmap_file_oom` panic, unrelated) as M0 — no new regression. 6 KernelLock host
tests pass; clippy clean on smp-shared + default + akuma-exec.

**Files:** `crates/akuma-exec/src/sync.rs` (KernelLock + tests), `.../src/bkl.rs` (new),
`.../src/lib.rs`, `.../Cargo.toml` + `.../build.rs` (smp-shared feature),
`.../src/process/mod.rs` (enter_user_mode), `src/exceptions.rs` (syscall wrapper),
`Cargo.toml` (forward smp-shared to akuma-exec).

## M2 — Shared scheduler: BKL on the IRQ path + per-core idle (in progress)

### M2a — IRQ/scheduler-path BKL + eret reconcile ✅ (2026-07-19)

Completes the BKL wiring M1 started (which covered only the syscall excursion). The
IRQ/SGI entry (`rust_irq_handler_with_sp`, `src/exceptions.rs`) now:
- `enter_kernel()` at the top, so the scheduler + device handlers run holding the BKL;
- after the handler, **reconciles the BKL to the EL the pending `eret` will enter** by
  reading the SPSR of the frame it's about to restore — `new_sp`'s frame after a
  context switch, else the interrupted `current_sp` frame. SPSR is at a fixed offset
  (`IRQ_FRAME_SPSR_OFFSET = 248`) derived from the `irq_el0_handler`/`irq_handler` save
  order and matched by `setup_fake_irq_frame`'s synthetic frames. `(spsr & 0xf) == 0`
  ⇒ returning to EL0 ⇒ release; else keep held.

Since every context switch (yield/block/preempt) goes through this one path, and the
BKL is held across the *entire* excursion (scheduler decision **and** the context
save), the switch is cross-core-atomic: a thread is fully switched out before any peer
core can touch it. `idle_halt` (`threading/mod.rs`) now drops the BKL before its `WFI`
and re-takes it after, so an idle core never blocks peers from entering the kernel. All
no-ops unless `cfg(kernel_smp_shared)`.

Verified SMP=2 (secondaries still parked): BSP boots, syscalls + timer preemptions flow
through the IRQ-path BKL, `smp_shared_cores_online PASSED`, zero `[BKL] stuck`, same
pre-existing baseline. No regression.

### M2b — SMP-safe scheduler: per-core idle ✅ (2026-07-19)

`ThreadPool::schedule_indices` now takes a `core_id` and supports **one idle thread per
core** (`IS_IDLE_THREAD` + `IDLE_SLOT_FOR_CORE[MAX_CORES]`, `register_core_idle`): the
round-robin scan **skips idle threads**, and a core with nothing else READY falls back
to *its own* idle — so one core can never grab another core's idle. The commit logic was
extracted into `commit_switch` and shared by the normal and idle-fallback paths. Slot 0
is registered as core 0's idle at init, so single-core behavior is unchanged (`core_id`
is 0, the fallback picks slot 0 — exactly the old "drop to idle" behavior). The
no-double-run guarantee comes for free from the BKL serialization + RUNNING state (M2a),
so no per-thread owner-core field was needed yet.

Verified SMP=2 (BSP): boots, futex + thread-cleanup tests pass, 114 akuma-exec host
tests pass, zero `[BKL] stuck`, same baseline. No regression. `current_idx` left as-is
(a near-dead mirror; only `validate_current_sp` reads it) — its removal is cosmetic and
deferred.

**Files:** `src/exceptions.rs` (IRQ wrapper + SPSR offset), `crates/akuma-exec/src/`
`threading/mod.rs` (schedule_indices core_id + per-core idle + commit_switch + idle_halt
BKL), `threading/types.rs` (MAX_CORES), `bkl.rs` (current_core_id all-configs).

### M2c — secondaries run the shared scheduler ✅ (2026-07-19)

Secondaries leave M0's WFE park and join the one shared scheduler. Each secondary
(`secondary_shared_start`): adopts its boot context as *its own* idle thread
(`adopt_current_as_core_idle` → a fresh slot, `register_core_idle`, `TPIDRRO_EL0`), does
its per-PE GIC receive-path init (CPU interface + redistributor wake + scheduler SGI 0 +
timer PPI 27 — device space is identity-mapped via boot L1[0], so no page-table change),
installs the shared `exception_vector_table` (`VBAR_EL1`), arms the shared 10 ms CNTV
tick (`timer::enable_timer_interrupts`), enables IRQs, and enters an idle loop. The
shared `timer_irq_handler` now rings the **current** core's scheduler SGI
(`gic::trigger_sgi_self`, gated `kernel_smp_shared`); `trigger_sgi_core` /
`trigger_sgi_self` are exposed for `any(kernel_smp, kernel_smp_shared)`. The runtime's
`trigger_sgi` (used by `yield_now`/`schedule_blocking`) is also self-targeted under
shared SMP. Demo: `spawn_worker_demo` spawns `cores+1` kernel workers that bump a
per-core counter then `sleep_us`; the boot self-test confirms they run on >1 core.

**Verified:** `smp_shared_scheduler PASSED` — SMP=2 "workers ran on 2 cores
(core0=399 core1=375)", SMP=4 "workers ran on 4 cores". Reaches the same pre-existing
baseline (the unrelated `test_mmap_file_oom` panic); ~16 transient BKL-contention
warnings during the test (a core spinning ≤10 M for the lock, then progressing — coarse
but correct), and post-panic orphan spinning once that pre-existing panic kills the BSP.

**Four bugs found + fixed along the way (all the hard part of M2):**
1. **16 KiB secondary stack overflow.** The idle stack backs the full IRQ excursion
   (trap frame + shared `timer_irq_handler` + kernel-timer + scheduler), far deeper than
   M0's park → fault-in-fault while holding the BKL. Bumped to 64 KiB (`STACK_SHIFT` 14→16).
2. **BKL acquire re-entrancy self-deadlock** (`owner=N waiter=N`). The syscall-path
   `enter_kernel` runs with IRQs enabled, so a timer IRQ nests mid-spin and *its*
   `enter_kernel` wins the lock for this core; the outer spin then retried `CAS(0, me)`
   forever. Fix: re-check `owner == me` every loop iteration in `KernelLock::acquire`.
3. **Voluntary reschedules rang PE0, not self.** `runtime().trigger_sgi` was
   `gic::trigger_sgi` (hardcoded PE0), so a secondary's `yield_now`/`schedule_blocking`
   poked the BSP and never rescheduled itself. Fix: self-target under `kernel_smp_shared`.
4. **Idle contention livelock (SMP=4) vs. no-switch.** A `yield_now()` in the idle loop
   hammered the BKL (idle cores fighting); removing it broke worker pickup because
   `idle_halt` disables preemption (blocking the timer from switching idle→worker). Fix:
   the secondary idle releases the BKL, WFIs, re-acquires — **without** disabling
   preemption — so the timer's self-SGI preempts idle onto a READY thread, cheaply.

**Deferred within M2 → folded into M3** (only needed once *user* address spaces run
cross-core): inner-shareable TLB flushes (`...is`) and real per-AS ASIDs. Kernel threads
all share the boot TTBR0, so the context switch's TTBR0 doesn't change and the local
`tlbi` is harmless here.

**Files:** `src/smp_shared.rs` (secondary scheduler bringup + GIC + worker demo),
`crates/akuma-exec/src/threading/mod.rs` (`adopt_current_as_core_idle`, `sleep_us`),
`crates/akuma-exec/src/sync.rs` (acquire re-entrancy fix), `src/gic.rs` + `src/gic_v3.rs`
(`trigger_sgi_core`/`trigger_sgi_self` gates), `src/timer.rs` (self-SGI), `src/main.rs`
(runtime `trigger_sgi`), `src/process_tests.rs` (`test_smp_shared_scheduler`).

## M3 — Userspace processes run across cores ✅ (2026-07-19)

**Goal:** user processes execute on secondaries — the real payoff, since userspace holds
no BKL, so this is *genuine* parallelism (unlike kernel threads, which the BKL serializes).

**Cross-core TLB coherence** (the one real prerequisite). In M2c all kernel threads shared
the boot TTBR0, so the context-switch's local `tlbi` sufficed. Once a *user* address space
runs on core 1 while core 0 edits its page tables (demand-paging, fork/CoW, munmap), core
0's core-local `tlbi` never reaches core 1 → stale TLB. Fix: under `cfg(kernel_smp_shared)`
the modification flushes broadcast over the inner-shareable domain —
`flush_tlb_all` → `vmalle1is`, `flush_tlb_asid` → `aside1is`, `flush_tlb_page` → `vaae1is`,
`flush_tlb_range_all_asid` → `vaae1is` per page (`crates/akuma-exec/src/mmu/mod.rs`). Other
builds keep the cheaper local form; the context-switch flush stays local (it only clears
the switching core's own TLB). **ASIDs deferred** (still ASID 0 for all): correctness holds
because per-core TLBs are private, the switch flushes the whole local TLB, and edits
broadcast — verified with no faults. Real per-AS ASIDs remain a *performance* follow-up.

**Instrumentation + demo.** `record_el0_trap` bumps a per-core counter on every EL0 trap —
from the syscall entry (`rust_sync_el0_handler`) *and* from the IRQ path when the
interrupted frame's SPSR shows EL0 (so a pure compute loop that only gets timer-preempted
still counts). `test_smp_shared_userspace` spawns two `/bin/hello` processes (each loops
printing with an 80 ms delay → periodic syscalls + `sleep_ms`, so it both traps and
migrates), waits with yield+`idle_halt`, and asserts userspace ran on >1 core. A spawned
process is a normal READY slot in the shared thread pool, so the shared scheduler places it
on any core.

**Verified:** SMP=2 "userspace ran on 2 cores (core0=600 core1=490 EL0 traps)"; SMP=4
"userspace ran on 4 cores". Reaches the same pre-existing baseline (the unrelated
`test_mmap_file_oom` panic) — no new fault from the IS-TLB change or cross-core userspace.
~17 (SMP=2) / ~45 (SMP=4) transient BKL-contention warnings before that panic. clippy clean;
114 host tests pass.

**Note on process-table safety:** under the BKL a syscall on any core has exclusive kernel
access, and a process isn't freed while its own thread is mid-syscall — so the ~218
`lookup_process` sites remain sound cross-core here without the refcount rework (that's the
M5 fine-graining, needed only when the BKL is split).

**Files:** `crates/akuma-exec/src/mmu/mod.rs` (inner-shareable flushes),
`src/smp_shared.rs` (`record_el0_trap` + user counters), `src/exceptions.rs` (count EL0
traps from syscall + IRQ paths), `src/process_tests.rs` (`test_smp_shared_userspace`).

## M4 — Migration + cross-core wakeups + hardening ✅ (2026-07-19)

**Migration** already fell out of the shared scheduler (M2c/M3 showed *different* threads
on different cores); M4 proves a *single* thread migrates. `test_smp_shared_migration`
spawns one probe (`migration_worker`) that records each core it runs on (MPIDR) between
short sleeps; asserts `count_ones() >= 2`. **Verified SMP=4: "one thread ran on 4 distinct
cores."** Correct because the context switch saves/restores per-thread SP+TTBR0 (shared
`THREAD_CONTEXTS`) and the M3 inner-shareable flushes keep a migrated address space
coherent. Slot-leak fix along the way: the demo workers/probe now self-terminate on a stop
flag (`DEMO_STOP` → `mark_current_terminated`) and are reclaimed (`stop_and_reclaim_demos`)
after each self-test, so they don't exhaust the scarce system-thread slots (RESERVED_THREADS).

**Cross-core wakeup — built, then deliberately NOT fired (deferred to M5).** Infrastructure:
an idle-core bitmask (`CORE_IDLE_MASK`, set/cleared around the secondary idle WFI) +
`wake_remote_idle` (rings the lowest idle peer's scheduler SGI) + a runtime hook. But
firing it from `wake()` on every wakeup measured a **~40x jump in BKL-contention spins**
(45 → 1843 stuck warnings at SMP=4): under a coarse BKL, a woken idle peer can't enter the
kernel until the waker releases the lock anyway, so it just spins. Removing the call
restored ~53 spins with all tests still passing (woken threads run via the waker's own
reschedule + the ≤10 ms tick). The mechanism stays wired for M5, where fine-grained locks
make waking an idle peer actually parallel. Lesson: eager cross-core wakeup is an
anti-optimization under a BKL.

**Hardening verified:** SMP=2 and SMP=4 run the full self-test suite (scheduler + userspace
+ migration) with no deadlock and low, non-growing contention, reaching the same
pre-existing baseline; heavy virtio-blk FS init completes under SMP=4; clippy clean across
smp-shared / devbox-smoltcp / default; 114 host tests pass.

**Open item — full devbox-smoltcp boot to userspace under SMP (investigated, not yet fixed).**
SMP=1 boots fully to userspace (herd/httpd/SmolNet); SMP≥2 stalls. gdbstub (lldb on :1235)
root-caused **two** distinct problems, both real:

1. **Bringup / `threading::init` ordering + slot timing.** `smp_shared::bringup_secondaries`
   runs *before* `threading::init` in `kernel_main`. With that order the async-main thread
   spawns into a slot that collides with a secondary's adopted idle slot (observed: async
   main = "tid=1" == a secondary's "idle tid 1"), and boot stalls with both cores idle and
   the async thread never running. Moving bringup to *after* `threading::init` fixes the
   collision (async main becomes tid=4, distinct) and lets the devbox get much further —
   **herd + httpd actually start** — BUT it regresses the boot-self-test build's contention
   badly (~50 → ~2900 `[BKL] stuck`, tests don't complete). So the reorder is not a clean
   fix and was reverted; the interaction between bringup timing and BKL contention needs a
   proper solution (`ThreadPool::init` only touches slot 0, so it does not itself clobber
   secondary slots — the collision is a timing/claim-order effect worth pinning down).

2. **Network stack is not SMP-safe (the deeper blocker).** Even with #1 worked around, the
   full boot then deadlocks in the socket/network path: right after `httpd` does
   `socket()`+`bind(port=8080)`, the BKL wedges (`owner` cycling between cores). This is the
   classic "NETWORK spinlock held across a scheduler switch" hazard (see
   `runbooks/debug-network.md` / `debug-ssh-latency.md`) now going cross-core: the async
   net-poll thread and a userspace socket syscall on different cores contend the `NETWORK`
   lock vs. the BKL. Fixing it means making the network path SMP-safe (drop-before-yield
   discipline enforced cross-core, or fold `NETWORK` under the BKL ordering) — natural M5
   (fine-grained locking) work.

Also note: on `devbox.img` the herd config pins `sshd`/`core2herd` to cores via `core_init`
(a *multikernel* mechanism), so they fail to start under shared SMP regardless — that disk's
herd `sshd.conf` needs re-provisioning for shared SMP (the task-2 follow-up).

**Status:** the boot self-tests (M0–M4) pass at SMP=2/4; the *full devbox boot to sshd* under
SMP needs the two fixes above. Run the devbox image single-core until then. Userspace itself
runs + migrates cross-core (M3/M4 tests prove it).

**Files:** `crates/akuma-exec/src/runtime.rs` (`wake_remote_idle` hook),
`crates/akuma-exec/src/threading/mod.rs` (`sleep_us`, wake note), `src/smp_shared.rs`
(idle mask + wake + migration/demo workers + reclaim), `src/main.rs` (wire hook),
`src/process_tests.rs` (`test_smp_shared_migration` + reclaim), `children.rs` (test rt field).

## M5a — Network SMP-safety + devbox boots to sshd under SMP ✅ (2026-07-19)

Resolves the M4 "open item": the full devbox-smoltcp boot to userspace/sshd now works
under `SMP=2` (reliable) and `SMP=4` (works; boot race noted below). SSH-in verified.
Three independent fixes, matching the two root causes M4 identified plus a stale-disk
issue found along the way:

1. **Network path made SMP-safe (`PreemptGuard`).** Root cause of the httpd
   `socket()`/`bind()` wedge: the base `NETWORK` (`smoltcp_net.rs`) and `SOCKET_TABLE`
   (`socket.rs`) `spinning_top` spinlocks disable no preemption of their own. Under the
   BKL, a holder timer-preempted mid-section is descheduled with the inner lock still
   held; the BKL is released on the switch's EL0 return, and the next core to enter the
   kernel spins on that inner lock **while holding the BKL** — so the BKL owner can never
   be rescheduled to release it. Deadlock. Fix: a `PreemptGuard` RAII
   (`akuma-net/src/runtime.rs`) disables scheduler preemption for the whole critical
   section (`poll`, `with_network`, `with_table` — the only base-lock sites; every other
   net path nests under one of these), so the inner lock is never held across a switch
   and under the BKL is never cross-core contended (the BKL already provides the mutual
   exclusion). Zero-cost no-op on every non-`smp-shared` build (compiles to nothing); the
   two `disable_preemption`/`enable_preemption` fn pointers are added to `NetRuntime` and
   wired at both registration sites (`main.rs`, `smp.rs`). Proof: under SMP httpd now
   completes `socket`+`bind`+`listen`+`accept` where it previously wedged the BKL.

2. **Secondary bringup ordering (the async-main slot collision).** For the RUNTIME image
   (`no-tests`, devbox) `smp_shared::bringup_secondaries()` now runs **after**
   `threading::init()` (right after preemptive scheduling is enabled in `kernel_main`),
   so each secondary's `adopt_current_as_core_idle` claims a slot from the initialized
   allocator and never collides with the async-main thread spawned later. The boot
   SELF-TEST image keeps the pre-init order (gated `not(feature = "no-tests")`): its
   `test_smp_shared_*` suite needs the secondaries online before it runs, and moving
   bringup after `init` makes them join the scheduler during the spawn-heavy suites and
   storm the coarse BKL. `adopt_current_as_core_idle` takes only `IrqGuard` + `POOL.lock`
   (never the BKL) and `bringup_secondaries` blocks until every secondary has adopted, so
   the post-init placement is race-free w.r.t. the async-main slot.

3. **Async-main network loop no longer hogs the BKL.** Even booted, the `run_async_main`
   poll loop (`main.rs`) held the BKL near-continuously on its core (nothing else READY
   there → `yield_now` re-runs it immediately), starving a userspace thread (sshd, or the
   login shell it forks) on a PEER core — the devbox booted but SSH sessions couldn't
   progress. Fix: under `all(kernel_smp_shared, feature = "smoltcp")`, after the
   `while poll()` drain loop (which already drained every ready packet), the loop drops
   the BKL and `WFI`s until the next interrupt (mirroring the secondary idle loop). A
   pending RX/timer/SGI IRQ makes `WFI` return at once, so burst draining is unaffected,
   while the peer core gets a BKL window every iteration. This took BKL-contention
   warnings during an SSH session from thousands to **zero**.

Also required (not a kernel change): the `devbox.img` herd config was stale — its
`/etc/herd/enabled/sshd.conf` pinned sshd to a core via `core_init` (a *multikernel*
mechanism that fails under shared SMP: `core_init failed for sshd — not started`), and a
`core2herd.conf` was present. Patched in place with `debugfs` to the unpinned overlay
config (`overlays/devbox/rootfs/.../sshd.conf`, `command = /bin/sshd`, `--port 22`, no
`core_init`) and `core2herd.conf` removed. The disk should be re-provisioned from the
overlay (`overlays/devbox/bootstrap.sh`) for reproducibility.

**Verified:** `SMP=2` devbox-smoltcp boots to userspace; `ssh -p 2222 root@localhost`
returns `uname`/`ls`/`free` across repeated sessions with **0** `[BKL] stuck`. `SMP=4`
also boots + SSHes in (`0` stuck on a clean boot). Boot self-tests (`--features smp-shared`)
still green at `SMP=2` (`smp_shared_cores_online/scheduler/userspace/migration` all
PASSED). 114 akuma-exec + 32 akuma-net host tests pass; clippy clean on
devbox-smoltcp / smp-shared / default.

**Residual (→ M5 fine-graining, coarse-BKL contention):** `SMP=4` has a *nondeterministic*
bringup/contention race — some boots wedge with one core holding the BKL, and the
`parallel_processes` boot self-test intermittently times out under heavy contention
(`stuck` spikes; a re-run passes). This is the coarse BKL scaling past 2 cores, the exact
"bringup/contention interaction" M4 flagged. Splitting the BKL into per-subsystem locks
(the real M5) is the fix; `SMP=2` is contention-clean today.

**Files:** `crates/akuma-net/src/runtime.rs` (`PreemptGuard` + fn ptrs), `.../smoltcp_net.rs`
+ `.../socket.rs` (guard the 3 base-lock sites), `.../Cargo.toml` + root `Cargo.toml`
(`smp-shared` feature forward), `src/main.rs` (bringup reorder + async-main BKL-release +
`NetRuntime` fields), `src/smp.rs` (`NetRuntime` fields).

## M5 (planned)

See the approved plan for the full roadmap: M2 shared scheduler (SMP-safe SGI handler,
per-core idle, inner-shareable TLB flushes `...is`, real per-AS ASIDs, map all GICR
frames), M3 userspace on secondaries, M4 migration + cross-core wakeups, M5
fine-grained locking (PMM/heap out first, then the `&'static mut Process` refcount
rework, then VFS/net).

## M5b — BKL-free user page-fault path (per-AS lock) — IN PROGRESS

Attacks the SMP=4 residual: a live SMP=4 self-test run measured all four `smp_shared_*`
tests PASSING but **~102 transient `[BKL] stuck` events (99 `owner=1`)** before the
pre-existing `test_mmap_file_oom` baseline panic — one core holding the BKL ~10 ms at a
time during FS/mmap-heavy work while the other three spin. Root of the win: run the
expensive part of demand paging (file block I/O, page copy/zero) **without** the BKL.

Design (approved; full doc: [`SMP_SHARED_M5_FAULT_LOCK_PLAN.md`](SMP_SHARED_M5_FAULT_LOCK_PLAN.md)).
Two naive shortcuts were rejected as unsound: (1) dropping the BKL while keeping
`&'static mut Process` is aliasing UB; (2) dropping the BKL around block I/O turns every
other `BLOCK_DEVICE.lock()`-under-BKL site into a deadlock. Investigation also found the
only AS structure NOT already self-locked is the **raw page-table memory** (edited by
`map_user_page*` / `AddressSpace::{un,}map_page` / `update_page_flags`); `user_frames`,
`page_table_frames`, `mmap_regions` (`vm_lock`), CoW refcounts, and lazy regions all have
their own locks. So the fix is a **per-address-space page-table lock** (`Process::as_lock`)
that the fault path takes *instead of* the BKL and every AS-mutating syscall takes *in
addition to* the BKL. Key simplification: syscalls keep the BKL, so `as_lock` only ever
arbitrates fault-vs-syscall and fault-vs-fault (never syscall-vs-syscall). `as_lock` is
held **IRQs-off** for short windows only (never across alloc/block-I/O/switch): IRQs-off
prevents a nested timer IRQ from acquiring the BKL behind a fault's back (an `as_lock`→BKL
inversion) and prevents holding the lock across a context switch. Lock order:
`BKL > as_lock > {PMM, page_table_frames, user_frames, fault_mutex, BLOCK_DEVICE, ...}`.

### M5b Stage 1 — primitives ✅ (2026-07-19)

`Process::as_lock: Spinlock<()>` (shared across CLONE_VM members via the leader, keyed by
`tgid`, like `fault_mutex`); `Process::with_as_locked` (closure form, IRQs-off + lock, for
the `&self` fault path) and `AsLockHold`/`Process::as_lock_hold` (RAII, for the
`&mut Process` syscall paths where a closure conflicts with the disjoint
`&mut self.address_space` edits); `lookup_process_shared -> &'static Process` (fault path,
avoids the `&mut` aliasing UB — all AS mutations it needs are `&self` or free functions).
All no-ops / uncompiled unless `cfg(kernel_smp_shared)`. Compiles both configs; 114
akuma-exec host tests pass.

### M5b Stage 2 — wire `as_lock` into AS-mutating syscalls ✅ (2026-07-19)

Every raw page-table edit *sequence* in `sys_mmap`/`sys_mremap`/`sys_munmap`/`sys_mprotect`/
`sys_brk`/`sys_madvise` now holds `as_lock` across the PTE writes + `user_frames`
bookkeeping, with **alloc and free kept OUTSIDE the hold** (the PMM OOM/reclaim path can
unmap pages and re-enter `as_lock` → self-deadlock if held across alloc; frees are
collected and run after release). `sys_mremap` and `sys_munmap` were restructured to
pre-allocate / collect-then-free; `set_brk` gained `AddressSpace::map_and_track` (the
"install" half of `alloc_and_map`) so its per-page grow allocs outside the hold. Faults
STILL take the BKL here, so `as_lock` is redundant with the BKL — **zero behavior change,
pure foundation**. Verified: clippy clean (smp-shared + default), 114 host tests,
`SMP=2` and `SMP=4` boot self-tests all PASS
(`cores_online`/`scheduler`/`userspace`/`migration`) with contention NOT regressed
(SMP=4 ~43 transient stuck this run vs the ~102 baseline, within boot-to-boot variance).

**Files:** `crates/akuma-exec/src/process/mod.rs` (`as_lock` field + `with_as_locked` +
`AsLockHold` + `set_brk`), `.../process/children.rs` (`lookup_process_shared`),
`.../mmu/mod.rs` (`map_and_track`), `src/syscall/mem.rs` (all AS-edit sequences),
`src/process_tests.rs` + `src/tests.rs` (Process literal field).

### M5b Stage 3 — all fault-path PTE edits under `as_lock` + file read/install split ✅ (2026-07-19)

Wired `as_lock` into **every** page-table-editing site in BOTH fault arms
(`EC_DATA_ABORT_LOWER` and `EC_INST_ABORT_LOWER`), faults still on the BKL (`as_lock`
redundant here — foundation for the Stage 4 flip). Needed because even disjoint fault
types race on shared *intermediate* page-table nodes, so no fault can go BKL-free until
all fault PTE edits honor the lock. Changes:

- **File-backed readahead split into Pass B / Pass C.** The old loop interleaved
  `vfs::read_at*` (block I/O) with `map_user_page_no_flush` per page. Now Pass B
  (no `as_lock`) allocs the pool + reads/fills + icache-maintains PRIVATE frames; Pass C
  (`as_lock`) atomically installs + tracks each filled frame, freeing install-race losers
  and unused pool frames after the hold. This is what makes the long part (block I/O)
  BKL-free once faults flip.
- CoW, PROT_NONE-commit, anon single-page, file single-page fallback, and eager-remap
  fallback edits all take `as_lock` (alloc/copy/IO/free kept outside; via
  `lookup_process_shared` + free fns so a shared `&Process` suffices).
- New mmu free fns (resolve L0 from `TTBR0_EL1`, so the fault path needs no `&mut`):
  `update_current_user_page_flags` (mprotect-style upgrade), `remap_current_user_page`
  (CoW overwrite — `map_user_page` refuses to replace a valid PTE).

**Verified:** clippy clean (smp-shared + default), 114 host tests. `SMP=2` fully clean —
all `smp_shared_{cores,scheduler,userspace,migration}` PASS (the userspace + migration
tests exercise cross-core demand paging through the restructured path). `SMP=4` end-to-end
run: all four tests PASS; threading tests pass 3/3 across reruns; transient `[BKL] stuck`
~44–54, comparable to Stage 2 (~51–60) — no contention regression. (Two initial SMP=4 runs
hit the pre-existing nondeterministic SMP=4 race in the spawn-heavy *threading* tests,
which don't touch the fault path; reruns of committed Stage 2 confirmed that race is
independent of these changes.)

**Files:** `crates/akuma-exec/src/mmu/mod.rs` (`update_current_user_page_flags`,
`remap_current_user_page`), `src/exceptions.rs` (both fault arms restructured).

**Known item for Stage 4:** Pass C currently holds `as_lock` (IRQs off) across the whole
readahead install batch (≤256 pages). Redundant with the BKL today, but once faults are
BKL-free this becomes a long non-preemptible window that would starve peers of the BKL —
chunk/per-page the install hold (bounded IRQ-off) as part of the flip.

### M5b Stage 4a — drop the BKL around file-fault block I/O ✅ (2026-07-19)

Faults keep the BKL, but the file-backed fault's **Pass B** (the Stage-3 fill pass:
frame-pool block I/O + fill + icache into PRIVATE frames) now runs with the BKL
**dropped** (`bkl::leave_kernel()` before the fill loop, `enter_kernel()` after), in both
fault arms. Peer cores can enter the kernel while this core waits on the disk — the
measured ~10 ms hold. Safe: Pass B touches only private frames + the block device (own
lock) + the held fault-slot; a concurrent `munmap` clears PTEs but never frees the
intermediate page-table pages the fill loop's `is_current_user_page_mapped` walk reads
(tables free only at teardown, which can't run while this thread faults). A timer may
re-acquire the BKL for us via the IRQ reconcile mid-fill; the `enter_kernel()` after the
loop is then idempotent, and the wrapper's final `leave_kernel()` balances it.

**Verified:** clippy clean (both configs), 114 host tests; `SMP=2` all `smp_shared_*` PASS
(19 stuck, == baseline); `SMP=4` 3/3 full runs PASS, ~46–55 stuck.

**Measurement finding (important for scoping 4b):** the SMP=4 self-test `[BKL] stuck`
count did **not** drop (Stage 3 ~44–54 → 4a ~46–55). Clustering the stucks by test phase
shows they occur in the **`smp_shared` scheduler/userspace tests + parallel-process
execution** (many cores spawning/scheduling under the coarse BKL) and right after the
threading suite — essentially **zero** are file-fault-bound. So the self-test benchmark is
scheduler/spawn-serialization-bound, which no fault-path change (4a or 4b) can move; M5b's
win (file-fault block I/O parallelism) only shows on file-mmap-heavy workloads. The
self-test `[BKL] stuck` count is therefore NOT a valid metric for M5b.

**Dedicated measurement — 4a validated (2026-07-19).** Added a runtime toggle
(`smp_shared::set_fault_bkl_drop_enabled`) for the Pass-B BKL-drop, a total-BKL-spin
counter (`sync::contention_spins`, accumulated once per contended acquire — a cross-core
wait-time proxy), and `test_smp_shared_fault_parallelism`: it spawns copies of the largest
on-disk binary (picks `/bin/busybox`, ~1.08 MB ⇒ ~256 ELF file-fault pages) concurrently
with the drop **OFF** then **ON**, comparing the spin counter. **SMP=2, 3 runs:** OFF
6.35M/6.12M/7.70M vs ON 4.26M/3.64M/5.26M → a stable **~32–40 % reduction** in cross-core
BKL wait with the drop on. Confirms 4a delivers even on HVF's host-cached disk (the fill
loop's 256-page read+copy+icache is enough CPU-under-BKL that moving it off matters). The
A/B ran at SMP=2 because the busybox spawn-storm provokes the pre-existing nondeterministic
SMP=4 contention race (observed in the drop-**OFF** phase, so unrelated to 4a).

**Files:** `src/exceptions.rs` (toggle-gated leave/enter around Pass B in both fault arms),
`src/smp_shared.rs` (drop toggle), `crates/akuma-exec/src/sync.rs` (contention-spin
counter), `src/process_tests.rs` (`test_smp_shared_fault_parallelism`).

### M5b — BKL-hold profiler + the scheduler finding (2026-07-19)

Retargeting per the 4a finding (self-test contention is scheduler/spawn-bound, not
fault-bound), added a **BKL-hold profiler** to attribute cross-core BKL *wait* to what the
*holding* core was doing. `crates/akuma-exec/src/sync.rs`: a per-core cache-line-padded
`HOLDER_TAG` (`set_holder_tag`, called from the syscall / fault / IRQ entry paths with the
syscall number / `HOLD_TAG_FAULT` / `HOLD_TAG_IRQ`) and a `WAIT_BY_HOLDER` histogram — a
waiter samples the blocking owner's tag once on first contention and adds its spin count to
that bucket on acquire. **Gated off by default** (`set_profiling`): its per-entry
`HOLDER_TAG` write false-shared across cores and tipped the flaky `parallel_processes` test
into a wedge; padding + the default-off gate fixed that (normal boot pays nothing;
`test_smp_shared_fault_parallelism` enables it only for its window).

**Finding (busybox ELF-fault storm, SMP=2, drop ON):** BKL wait by holder —
**IRQ/scheduler ≈ 70 %** (4.7–6.8 M spins), **FAULT ≈ 20 %** (~1.6 M), syscalls the rest
(<0.7 M). So the dominant cross-core BKL-hold under multi-process load is the
**IRQ/scheduler path** (the scheduler runs under the BKL on every timer tick / SGI — M2a),
not faults. That is the real lever for SMP contention and explains why fault-path work
(4a/4b) can't move the scheduler/spawn-bound self-test `[BKL] stuck` count. The natural
next milestone is a **run-queue lock split from the BKL** so scheduler decisions don't
serialize against unrelated kernel work. (4a's fault win stands — it addresses the ~20 %
FAULT slice; the ~33 % A/B from the cleaner profiler-off run vs ~6–8 % with profiling on
reflects HVF measurement noise + the profiler perturbing timing.)

**Files:** `crates/akuma-exec/src/sync.rs` (profiler + `set_profiling`), `src/exceptions.rs`
(`set_holder_tag` at syscall/fault/IRQ entry), `src/process_tests.rs` (profiler dump).

### M5b Stage 4b — reconcile-aware full flip (faults never hold BKL) — OPTIONAL/DEFERRED

Superseded in priority by the scheduler finding above: with the IRQ/scheduler path holding
~70 % of contended BKL time and faults ~20 %, splitting the run queue out of the BKL is the
higher-leverage next step than making the last ~20 % of fault work BKL-free.

Faults never take the BKL; use `as_lock` only, plus a fault-aware IRQ reconcile
(per-thread "in-BKL-free-fault" flag so a timer tick releases rather than acquires the BKL
for a faulting thread). Highest-risk change in M5b (touches the M2a reconcile path). Note
from 4a's measurement: its incremental benefit over 4a is small — anon/CoW faults are
µs-scale (not a contention source) and 4a already moved the dominant fault cost (block I/O)
off the BKL — so the risk/reward is under review.

### M5b Stage 4 — flip fault fast path BKL-free — AFTER 3

Route resolvable self-AS data-abort faults through `as_lock` instead of `enter_kernel`;
keep the BKL for the SVC arm and the slow paths (SIGSEGV/signals, foreign-`parent_pid`
lookups, OOM, kernel-VA faults). Wire `as_lock` into `exit`/`exec` AS teardown and `fork`
CoW-marking (teardown is delicate — `as_lock` lives *in* the Process being freed; the
group can't fully exit while a member thread is mid-fault). Add a `process_tests.rs`
self-test (two processes fault concurrently on distinct address spaces on distinct cores;
assert the BKL is not held during the fault window). Success metric: SMP=4 transient
`[BKL] stuck` count drops sharply vs the ~102 baseline; devbox-smoltcp SMP=2/4 boot + ssh
with 0 stuck.
