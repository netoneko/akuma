# Plan: Explain top's 95% CPU thread + add a CORE column to `top`

> **Note:** `userspace/top` has been removed as part of a codebase trimming effort (see `docs/TRIMMING_FAT_PART_3.md`). This document is kept for historical reference.

## Context

`top` showed one kernel thread (TID 2, PID 0) pinned at ~94% CPU. The user asked
(a) why, suspecting it's the network thread, and (b) to add a per-thread **core**
column to `top` — "useful for later" as SMP work continues.

### Answer to (a) — why TID 2 is at ~95%
TID 2 is the **network poller thread**: the `run_async_main` loop in `src/main.rs`.
Each iteration drains the smoltcp stack (`while akuma_net::smoltcp_net::poll()`, up
to 64 packets) then calls `threading::yield_now()`. When no other thread is READY,
`yield_now()` returns immediately, so the loop re-runs continuously and never truly
sleeps. The scheduler also **boosts** this thread (`NETWORK_THREAD_RATIO`). `top`
computes `CPU% = Δtotal_time_us / Δuptime`, so this de-facto idle consumer reads
~95%. It's a busy-poll/idle-consumer artifact, not real work. (On `kernel_smp_shared`
builds this same loop does a real `wfi` instead — see `src/main.rs:1538`.) This is
kernel behavior and is **not** changed by this task.

### The core-column blocker (and the decision taken)
No per-thread core is tracked or exposed today: `ThreadCpuStat` (the 48-byte struct
`top` reads via syscall 314) has only `tid,pid,box_id,total_time_us,state,_reserved[7],name`
and the kernel leaves `_reserved` zeroed; `ThreadSlot` (the TCB) has no core field.
So a truthful core column **cannot** be produced by editing `userspace/top` alone.

**Decisions confirmed with the user:**
- Relax the original "only `userspace/top`" constraint to allow the minimal kernel +
  libakuma change needed to actually expose the core.
- Store last-run core in a **lock-free atomic array** (`static LAST_CORE:
  [AtomicU8; MAX_THREADS]`), mirroring the existing `TOTAL_CPU_TIMES` /
  `THREAD_STATES` arrays — **not** as a `ThreadSlot` field, which would force
  `sys_get_cpu_stats` (top's read path) to take `POOL.lock()` on every refresh
  (the exact pattern the `USER_COPY_FAULT_HANDLER` comment at
  `crates/akuma-exec/src/threading/mod.rs:312-320` warns caused a spinlock hang).

## Changes

### 1. Scheduler: track last-run core (lock-free)
File: `crates/akuma-exec/src/threading/mod.rs`
- Add, next to `TOTAL_CPU_TIMES` (~line 334):
  ```rust
  /// Last core each thread ran on (MPIDR aff0). 0xFF = never scheduled.
  /// Lock-free like THREAD_STATES/TOTAL_CPU_TIMES so sys_get_cpu_stats reads it
  /// without POOL.lock (see USER_COPY_FAULT_HANDLER note above).
  static LAST_CORE: [AtomicU8; MAX_THREADS] = {
      const INIT: AtomicU8 = AtomicU8::new(0xFF);
      [INIT; MAX_THREADS]
  };
  ```
- In `commit_switch` (line 2200), right after `THREAD_STATES[next_idx].store(RUNNING…)`
  (line 2215), record the core the incoming thread is now running on:
  ```rust
  LAST_CORE[next_idx].store(crate::bkl::current_core_id() as u8, Ordering::Relaxed);
  ```
  `commit_switch` always runs on the core that will run `next_idx` (single runqueue,
  each core schedules itself), so `bkl::current_core_id()` is authoritative. It's a
  one-line change with no signature churn and covers both call sites (2181 idle,
  2192 normal). `bkl::current_core_id()` already returns `0` on non-SMP/host builds.
- Add a public accessor next to `get_thread_cpu_time` (line 895), modeled on
  `get_thread_state`:
  ```rust
  pub fn get_thread_last_core(idx: usize) -> u8 {
      if idx < MAX_THREADS { LAST_CORE[idx].load(Ordering::Relaxed) } else { 0xFF }
  }
  ```

### 2. Wire format: add `last_core`, keep 48-byte ABI
Change `_reserved: [u8; 7]` → `last_core: u8` + `_reserved: [u8; 6]` in BOTH identical
struct definitions so they stay byte-identical (offset 25, no size change):
- `src/syscall/mod.rs:371-381` (kernel copy)
- `userspace/libakuma/src/lib.rs:122-132` (userspace copy)

### 3. Kernel stat populator
File: `src/syscall/term.rs`, in `sys_get_cpu_stats` (line 455) add to the `ThreadCpuStat`
initializer:
```rust
last_core: akuma_exec::threading::get_thread_last_core(i),
```

### 4. `top` display — the CORE column
File: `userspace/top/src/main.rs`
- Header (line 56): insert a `CORE` column (place it after `STATE`):
  `"TID  PID  BOX ID    STATE    CORE  CPU%   TIME(ms)  NAME"` (adjust the `----`
  separator width at line 57 to match).
- In the row loop (after `print(state_str)` at line 94), print the core:
  `0xFF` → show `"-"` (never scheduled); otherwise `print_u32_fixed(cur.last_core as u32, 3)`.
  Reuse the existing `print_u32_fixed` helper (main.rs:142).

### 5. Boot self-test (required by CLAUDE.md for kernel/syscall changes)
File: `src/process_tests.rs` — add e.g. `test_thread_last_core_tracked()` and register it
alongside the other tests. Spawn/select a thread, run the scheduler, then assert
`threading::get_thread_last_core(tid)` for a RUNNING thread is a valid core id
(`< MAX_CORES`, and `== 0` on the single-core boot path) — i.e. no longer the `0xFF`
sentinel. Model it on the existing thread tests in that file.

## Verification
1. `cargo check` then `cargo build --release`.
2. Host crate tests (akuma-exec changed):
   `cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)` — plus the pre-commit
   hook runs clippy + tests.
3. Boot and confirm the new self-test passes in the boot suite output.
4. `cargo run --release`, SSH in (`root@localhost -p 2222`), run `top` (or `top --once`):
   confirm a `CORE` column renders and shows `0` for threads on the single-core build
   (`-` for any never-scheduled slot). The ~95% network thread is expected and unchanged.
5. (Later, SMP) Boot with SMP=2/4 and confirm the CORE column shows differing core ids
   across threads.

## Out of scope
The ~95% network-thread CPU is explained but not "fixed" — that's kernel scheduler
behavior (busy-poll + boost), separate from this display task.
