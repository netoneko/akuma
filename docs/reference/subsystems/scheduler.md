# Scheduler / threads / concurrency

Current-state architecture for preemptive threading, the scheduler, context
switch, synchronization primitives, and blocking.

> **Stability: A (stable).** Threading churn was concentrated in Jan–Mar and
> has been quiet since. The proportional scheduler + SGI preemption model is
> settled. The recurring lesson across the codebase: **never `yield_now()` or
> do slow I/O inside a preemption-disabled closure** — that is the single most
> common cause of hangs (see `../../runbooks/debug-network.md`).

For SMP/multikernel (one-kernel-per-core), see the last section.

## Threading model

- **Preemptive**, fixed-size thread pool. `MAX_THREADS`=64 slots (`config.rs`);
  runtime cap `THREAD_LIMIT` clamped to `[RESERVED_THREADS+1, MAX_THREADS]`.
- **Stacks** come from PMM (`alloc_pages_contiguous_zeroed`); per-thread sizes
  via `USER_THREAD_STACK_SIZE` / `SYSTEM_THREAD_STACK_SIZE` (`config.rs`).
  Stack-overflow detection via canaries (`ENABLE_STACK_CANARIES`).
- **Thread states** (`crates/akuma-exec/src/threading/types.rs:126`):
  `Free → Ready → Running → Terminated`. Tracked in lock-free atomics
  `THREAD_STATES: [AtomicU8; MAX_THREADS]` (no mutex on the hot path).

## Preemption

- **10 ms timer → SGI** (Software Generated Interrupt). The timer fires, the
  exception handler runs the scheduler.
- `PREEMPT_WAKE_TID` / `WOKEN_STATES`: "sticky wake" flags set by `wake()`,
  consumed in `schedule_indices`. A woken thread runs promptly via the SGI.
- **Preemption watchdog** (`ENABLE_PREEMPTION_WATCHDOG`, default true): warns
  if preemption disabled >100 ms, **panics** at 5 s (`types.rs:42-45`). This is
  the tripwire for the "yield inside a critical section" bug class.
- `with_irqs_disabled` (`runtime.rs:256`) is the primitive; nesting is counted
  (`preemption_disabled_count`).

## Context switch

- Saves/restores the full trap frame (`THREAD_CURRENT_TRAP_FRAME` per slot) +
  callee-saved regs + TPIDR_EL1 (exception SP) + TTBR0 swap.
- **Key invariant:** `flush_tlb_asid(0)` (all ASIDs) after a TTBR0 change that
  affects shared L0 entries — sibling threads share L0 with different ASIDs.
  Stale TTBR0 was the dominant bug class in `fork`/`vfork`/`clone_thread`
  (all three call sites now set `child_ctx.ttbr0 = new_proc.address_space.ttbr0()`).

## Synchronization primitives

| Primitive | Where | Use |
|---|---|---|
| `with_irqs_disabled(f)` | `runtime.rs:256`, `irq.rs:46` | The base. Disables preemption for a critical section. Nesting counted. |
| `Spinlock<T>` (`spinning_top`) | everywhere | Kernel locks. Always held briefly; IRQs may or may not be disabled — combine with `with_irqs_disabled` if the holder can be interrupted. |
| `with_device(f)` | `src/block.rs:294` | Block-device critical section — **disables preemption before acquiring the spinlock** (the priority-inversion fix). |
| `with_fs` / `with_network` / `with_socket_handle` / `with_table` | syscall/net layers | VFS/network/socket-table critical sections. Same rule: disable preemption, **never `yield_now()` or do slow I/O inside**. |

### The load-bearing rule

> A preemption-disabled closure (`with_fs`, `with_network`,
> `with_socket_handle`) must not `yield_now()`, `schedule_blocking()`, or call
> slow synchronous I/O. If you must, snapshot state, drop the closure, then
> yield/block. Violating this is the #1 cause of whole-system hangs.

## Blocking & wait/wake

- **`yield_now()`** (`threading/mod.rs:2221`) — voluntary yield; thread stays
  `Ready`, wakes promptly, **no SGI**. Use inside `block_on` and any
  cooperative loop.
- **`schedule_blocking(wake_time_us)`** (`threading/mod.rs:2555`) — flips the
  thread to `Waiting`; on wake fires an SGI for immediate context switch.
  **Do NOT use inside `block_on` while the network thread may hold the
  `NETWORK` spinlock** (SGI-during-poll deadlock — see
  `../../runbooks/debug-ssh-latency.md`).
- **`Waker`** (`threading/mod.rs:2470`) — `wake()` sets the sticky
  `WOKEN_STATES` flag + `PREEMPT_WAKE_TID`. Used by wait queues, epoll, and the
  SSH `current_thread_waker()`.
- **Wait queues** (see `archive/WAIT_QUEUES.md`): a thread registers a `Waker`
  on a wait queue, then `schedule_blocking()`. The producer fires the waker.
  Classic blocking-syscall pattern (pipes, poll, read on a fd).

## Scheduling weights

- **Proportional scheduler** (`schedule_indices`, `threading/mod.rs:1874`).
  `MAIN_THREAD_PRIORITY_BOOST` is legacy (off) — proportional is default.
- **`NETWORK_THREAD_RATIO`** (default 4) — the network thread is boosted every
  N ticks. The boosted thread is the **registered** one
  (`set_network_thread_id`, called by `run_async_main`), not slot 0.
  Historical bug: boost was hardcoded to slot 0 (idle) → SSH staggering.

## SMP / multikernel

Behind `cfg(kernel_smp)` (the `smp` feature), paired with the `release-smp`
profile. One kernel **per core** — not SMP sharing.

- Secondary cores boot to **PARKED** (`wfe` loop). herd activates them via the
  `core_init` syscall (`MSG_CORE_INIT`), which PSCI `CPU_ON`s the core.
- Cross-core IPC via the `akuma-smp` message bus (`MSG_FWD_SYSCALL_REQ` /
  `MSG_FWD_ECHO_REQ`); forwarded `openat`/`read`/`close` let a pinned core's
  process reach core 0's filesystem.
- Per-core PMM/heap/POOL/TALC isolation; smoltcp on BSP only (DNS/HTTPS/entropy
  forwarded to core 0).
- Config: `MULTIKERNEL_INIT_HERD` + `AUTO_START_HERD` (both default on =
  userspace-driven). Parked watchdog 120 s. Box + non-BSP core mutually exclusive.
- Demo: [`acceptance/12_multikernel_demo.md`](../../../acceptance/12_multikernel_demo.md).

## Background

- `archive/MULTITASKING.md`, `archive/CONCURRENCY.md`, `archive/LOCK_REFERENCE.md`.
- `archive/WAIT_QUEUES.md`, `archive/SYSCALL_BLOCKING.md`.
- `archive/CONTEXT_SWITCH_FIX_2026.md`, `archive/TTBR0_AND_THREADING_FIXES.md`.
- `archive/MULTIKERNEL.md` (29 commits — the SMP design).
