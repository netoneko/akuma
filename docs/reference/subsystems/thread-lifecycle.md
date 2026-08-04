# Thread & thread-group lifecycle — states, edges, and the locks on every leaf

Current-state map of the two coupled state machines — **thread slots** and
**process (thread-group) slots** — with every lifecycle edge annotated with the
locks it takes, and the leaves traced to their lock sets. This is the diagram to
consult before adding any call on a teardown/exit/fault path.

> **Stability: C (active risk).** The leaf traces below found two live defect
> classes on 2026-08-02 (one fixed, one open — see §5). Re-verify the ⚠ tables
> against the code before relying on them.

For scheduling itself see [`scheduler.md`](scheduler.md); for lock discipline
rules see [`locking.md`](locking.md); for the process table see
[`memory.md`](memory.md) and `crates/akuma-exec/src/process/table.rs`.

## 1. The two state machines

```mermaid
stateDiagram-v2
    state "THREAD SLOT (THREAD_STATES, atomics)" as T {
        [*] --> Free
        Free --> Initializing : claim_free_slot CAS
        Initializing --> Ready : spawn or clone_thread
        Ready --> Running : schedule_indices (POOL try_lock, IRQ ctx)
        Running --> Ready : preempt or yield
        Running --> Blocked : schedule_blocking
        Blocked --> Ready : ThreadWaker.wake (lock-free)
        Running --> Terminated : exit, kill consumed, hard-terminate
        Terminated --> Initializing : cleanup_terminated CAS, 10ms cooldown
        Initializing --> Free : slot scrub + CLEANUP_CALLBACK
    }

    state "PROCESS SLOT (SLOT_STATES, atomics)" as P {
        [*] --> FreeSlot
        FreeSlot --> Active : register_process
        Active --> Retired : unregister_process CAS
        Retired --> FreeSlot : reclaim_retired_processes, cooldown, drops the Process box
    }
```

The coupling points:

- **Per-slot state is scrubbed by one function, `scrub_thread_slot`**
  (`threading/mod.rs`), called from *both* Free→Initializing claim paths
  (`claim_free_slot` and the direct claim inside
  `ThreadPool::spawn_user_closure_initializing`, which is the one every real
  `pthread_create` takes) and once more before a slot returns to `Free`. Before
  2026-08-04 those three sites each cleared a *different* subset, so a cloned
  thread inherited the previous occupant's `THREAD_SIGNAL_MASK`,
  `THREAD_RESTORE_SIGMASK_PENDING`, `WOKEN_STATES`, `USER_COPY_FAULT_HANDLER`
  and the `[THR-DUMP]`/`[PSTATS]` diagnostic registers. **Add new per-slot
  state to that function, never to a call site** — the drift is the bug.
  Excluded on purpose: `THREAD_STATES` (the caller's CAS owns it),
  `IS_IDLE_THREAD` (permanent property of idle slots), `ON_CPU`,
  `THREAD_CONTEXTS`. Background:
  [`../../archive/SELFHOST_DEVBOX_SMOLTCP.md`](../../archive/SELFHOST_DEVBOX_SMOLTCP.md)
  §"per-slot state inherited across thread-slot recycling".
- **Per-tid state owned by other subsystems is purged at death, not at recycle.**
  `threading::set_slot_purge_callback` registers a kernel hook (the tables live
  in the bin crate) invoked from **`mark_thread_terminated`** *and* the recycler.
  It must run at death because a slot stays `Terminated` for ≥10 ms — often far
  longer — and for that whole window a tid left in `FUTEX_WAITERS` is still a
  wake target: `futex_do_wake` pops it, counts it toward `max_wake`, and wakes a
  thread that will never run, consuming a `FUTEX_WAKE(uaddr, 1)` the real waiter
  needed. Dropping it early is safe only because a queue entry is of no further
  use to a dead thread — unlike its trap frame, kernel stack or sigaltstack,
  which the terminal park may still touch and which therefore wait for the
  recycler. **The rule: scrub slot registers at the ownership boundary, purge
  external registrations at death.** Do not move this cleanup "to the
  termination leaf" — see §4: the abandoned-stack leaves never run it.
- **A `TERMINATED` slot still occupies its index** for at least
  `THREAD_CLEANUP_COOLDOWN_US` (10 ms) *and* until some path runs a reclaim
  pass, so the usable ceiling under a spawn-heavy load is
  `ceiling − (deaths/sec × 0.01s)`, not `ceiling`. Measured: 43 of 56 slots
  held by corpses while only 13 threads were live. The `[threads]` census on
  the exhaustion path prints the live/terminated split precisely so this is
  distinguishable from genuine capacity; `threadmax` (`userspace/forktest/
  c_stress/`) measures both, and on a refusal retries after a settle to report
  which of the two it hit.
- **`MAX_THREADS` is defined once**, in `threading/types.rs` (256; 64 under
  `kernel_profile_size`), and `config::MAX_THREADS` is a `pub use` of it. It is
  the compile-time array size and the ceiling `set_thread_limit` clamps against
  — **not** the working limit, which `compute_thread_limit` derives from RAM at
  boot. The two used to be independent literals joined by a "must match"
  comment; raising only one silently did nothing (boot logged `Thread limit:
  256` while the census kept reporting `ceiling=56`). At 256 one process holds
  **244** simultaneous threads for +33 KB of `.bss`.
- `THREAD_PID_MAP` (Spinlock) binds tid → pid; written on spawn/clone/fork,
  erased on exit/kill/recycle. The `slot_still_owned_by` guard (dc4684a)
  re-reads it before acting on a pre-yield `thread_id` snapshot. It is the
  **sole** authority for thread identity when `VFORK_FASTPATH_ENABLED` (default
  true): `read_current_pid` consults it first and only falls through to the
  `PROCESS_INFO` page on a miss, so its accuracy across slot recycling is a
  correctness dependency, not a cache.
- `unregister_process` terminates the thread named by `p.thread_id` as a
  backstop for `kill_thread_group`'s grace-gap (see
  `../../archive/STALE_THREAD_SLOT_KILL.md` §5.1 — do **not** "tidy" that
  asymmetry away).
- A RETIRED process's memory is freed only when `drop(Box<Process>)` runs —
  by `netpoll_maint` (100 ms), an idle loop, terminal teardown, or the PMM
  pressure ladder (`process/reclaim.rs`).

## 2. Lifecycle edges and their locks

Lock legend: **M** = acquisition wrapped in `with_irqs_disabled`/`IrqGuard`;
**U** = unmasked; **P** = preemption disabled only (`LifecycleGuard`/
`PreemptGuard`, IRQs stay on).

| Edge | Entry points | Locks taken (in order) | Notes |
|---|---|---|---|
| spawn thread | `spawn_user_thread_initializing`, `spawn_*_fn` | `POOL` M | stack alloc from PMM **inside** the `POOL` hold (`threading/mod.rs:754,895`); slot-exhaustion reclaim-retry sits **outside** it — see §2.1 |
| clone thread | `clone_thread` | `THREAD_PID_MAP` M; `LAZY_REGION_TABLE` M | P held across the whole clone incl. **raw user writes** of parent/child tid (`process/mod.rs:2799,2803`) |
| fork | `fork_process` | `THREAD_PID_MAP` M; `SHARED_L0_TABLE` M | `SHARED_L0_TABLE` insert **allocates under the lock** (`mmu/mod.rs:421-430`) |
| execve | `do_execve` → `replace_image*` → `enter_user_mode` | VFS/ext2/block (BKL-dropped window); `as_lock` via exclusive access | **eret leaf** — see §4. Frame abandoned on success |
| run → terminated | `mark_thread_terminated`, kill-request consume | none (atomics) | consume site holds the **BKL** across the terminal yield loop |
| exit / exit_group | `sys_exit*` → `return_to_kernel` | `PROCESS_CHANNELS` M, `THREAD_PID_MAP` M, `LAZY_REGION_TABLE` M, pipe/VFS via `cleanup_process_fds` | P for most of it; robust-futex walk touches user memory under P; **`!` leaf** — see §4 |
| kill group P1 | `kill_thread_group` | none (atomics + wakers) | up to 2 s grace-wait under the caller's P |
| kill group P2 | same | pipe/VFS via `cleanup_process_fds`; `PROCESS_CHANNELS` M; `THREAD_PID_MAP` M | then `unregister_process` (below) |
| unregister | `unregister_process` | `THREAD_PID_MAP` M (recycled-slot proof) | CAS ACTIVE→RETIRED; `note_retired` atomics |
| slot recycle | `cleanup_terminated_internal` | `POOL` M (short holds) | `CLEANUP_CALLBACK` re-enters process subsystem (`THREAD_PID_MAP` ×3, `unregister_process`) with no lock held |
| retired reclaim | `reclaim_retired_processes` | none in sweep | `drop(Box<Process>)` = the whole §3 tree |
| pressure drain | `drain_retired_under_pressure` (from `alloc_page_zeroed_user`) | same as reclaim | ambient context = **whatever faulted** — see §5.2 |
| scheduler (IRQ) | `sgi_scheduler_handler_with_sp` | `POOL` **try_lock** M | never blocks; no other subsystem lock reachable from IRQ context |

### 2.1 Every spawn path reclaims before it reports exhaustion (2026-08-03)

A `FREE`-slot scan that misses does **not** mean the pool is full — far more
often the slots are `TERMINATED` and simply uncollected, because deferred
cleanup's steady-state collector is thread 0's *idle* loop, which a busy system
never reaches (`BKL_VFS_CARVE_OUT.md` §11.4: p50 24 s, max 192 s uncollected
against a 10 ms cooldown). Every spawn entry point therefore does **miss →
`reclaim_terminated_slots()` → retry once → still-miss → fail**:

| Entry point | Site | Claims from |
|---|---|---|
| `spawn_user_thread_fn_internal` | `threading/mod.rs:3347` | `reserved_threads..thread_limit()` |
| `spawn_system_thread_fn` | `threading/mod.rs:3197` | `1..reserved_threads` |
| `spawn_user_thread_initializing` | `threading/mod.rs:826-864` | `reserved_threads..thread_limit()` |

The third was added 2026-08-03 and is the one that covers **fork, vfork and
`clone_thread`** — i.e. every real `pthread_create` — since all three funnel
through it (`process/mod.rs:2494,2653,2782`). Until then that path had a single
linear scan and returned `EAGAIN` straight to userspace: a tight, correctly
`pthread_join`ed 200× `pthread_create` loop died at iteration ~58-68 of 200 with
`MAX_THREADS = 64`, while most of the pool sat `TERMINATED`.

**Placement is forced by the lock, not style.** The retry cannot live inside
`ThreadPool::spawn_user_closure_initializing` where the scan is: that method
runs with `POOL` held (`threading/mod.rs:837`), and `reclaim_terminated_slots`
→ `cleanup_terminated_internal` takes `POOL` itself (`:1227`, again at `:1270`
on the size profile). It has to go in the wrapper, outside the hold. This is the
thread-slot echo of the `register_process` rule in
[`locking.md`](locking.md#no-bkl-process): on-demand reclaim is safe exactly
where the caller controls its own lock context.

> ⚠ **Untested by the boot suite.** `test_thread_slot_reclaim_on_spawn`
> (`src/process_tests.rs:1273`) drives `spawn_user_thread_fn`, i.e. only the
> `_fn_internal` row. The `_initializing` row is covered only by the userspace
> repro (`userspace/forktest/c_stress/futextest.c` phase 2).

## 3. The `drop(Box<Process>)` lock tree (teardown leaf)

Every reclaim/drain leaf executes this transitive set
(full trace: `process/table.rs:263` → field drops):

```
drop(Box<Process>)
├─ Process::drop           → PMM (M, per frame)
├─ UserAddressSpace::drop  → SHARED_L0_TABLE (M) → TALC under it (map node dealloc)
│                            user_frames/page_table_frames (M, held ACROSS free loops)
│                            → PMM (M) + COW_REFCOUNTS (M) + FRAME_TRACKER (U, debug)
│                            ASID_ALLOCATOR (M)
├─ SharedFdTable::drop → close_all
│    ├─ fds.table (M) — values().cloned().collect() ALLOCATES under the lock
│    ├─ PIPES (M)                    pipe_close_read/write
│    ├─ SOCKET_TABLE (U,P) → NETWORK (U,P)   remove_socket → smoltcp close
│    ├─ EVENTFDS (M), EPOLL_TABLE (M), PIDFD_TABLE (M), CHILD_CHANNELS (M)
│    └─ RemoteFd → blocking cross-core forward (multikernel only)
└─ String/Vec/BTreeMap/Arc drops → TALC (heap lock) throughout
```

**13 distinct global locks** are reachable from a single `drop(Box<Process>)`.
Any context that can already hold one of them must never run this drop.

## 4. The three abandoned-stack leaves (`-> !`)

These are the special leaves the diagram must make visible: they end a kernel
stack **without unwinding it**. No destructor on the abandoned frames runs; no
lock guard on them is ever released.

| Leaf | Where | What is abandoned |
|---|---|---|
| `enter_user_mode` | initial launch, **every successful execve** (`syscall/proc.rs`) | the whole execve syscall stack |
| `return_to_kernel` / `_from_fault` | process exit, OOM kill (`alloc_error_handler`), fault kill | the interrupted syscall/fault stack, **including any lock guards it held** |
| `el1_fault_recovery_pad` | EL1 abort recovery (`exceptions.rs:1732`) | the faulting kernel frame — its `IrqGuard`s/`SpinlockGuard`s leak permanently |

Rules that follow (violations of each were found live):

1. **Nothing heap-owned may be live across an `eret` leaf.** Violation: execve
   leaked its whole-file ELF buffer (~1.1 MB per `busybox sh` exec) plus
   argv/env every success → kernel heap ratcheted to the ~1 GB wall under
   exec-heavy load (rustc hammer), ending in the `[OOM] … killing process`
   loop. **Fixed 2026-08-02** (`do_execve`/`exec_shebang` drop all heap locals
   before `enter_user_mode`). The same audit applies to anything added to
   `return_to_kernel*` upstream frames.
2. **`return_to_kernel*` and the pressure drain may re-acquire any §3 lock, so
   they must only run from contexts holding none of them.** The drain's module
   doc claims this is vetted; it is true for the idle/exit sites, **unprovable
   for the two hijack leaves** — `alloc_error_handler` and
   `el1_fault_recovery_pad` inherit an arbitrary ambient lock set. A failed
   allocation inside `pipe_write`'s buffer growth (under `PIPES` + IRQs
   masked — the class `syscall/pipe.rs:29-35` already documents) reaches
   `return_to_kernel` → teardown/drain → `PIPES.lock()` → **single-core spin
   with IRQs masked**: 100 % CPU, frozen serial, no panic output.

## 5. Open ⚠ leaf traces (as of 2026-08-02)

### 5.1 Demand-pager runs before the user-copy fixup

`rust_sync_el1_handler` order: CoW resolve → **demand-page**
(`try_resolve_el1_user_copy_lazy_fault` → `alloc_page_zeroed_user` → pressure
ladder → drain) → only then the `copy_*_safe` EFAULT fixup. Consequence:
`copy_*_safe` does **not** protect a lock-holding caller from demand paging.
The comments claiming otherwise are stale and wrong:
`syscall/sync.rs:24-31`, `pmm.rs:770-775`, `process/reclaim.rs` ("vetted
drain sites").

Known demand-page-capable user accesses under a held spinlock (all can enter
the §3 drain while holding their lock):

| Site | Lock held |
|---|---|
| `syscall/sync.rs:116` futex word read | `FUTEX_WAITERS` + IRQs masked |
| `syscall/msgqueue.rs:264-283` msg copies (unbounded len) | `MSGQUEUE_TABLE` + IRQs masked |
| `syscall/timerfd.rs:94-98,152-156` | `TIMERFD_TABLE` |
| `syscall/term.rs:195,241,257,338` | `TerminalState` |
| `syscall/signal.rs:42` old-sigaction copy | `SharedSignalTable.actions` |

### 5.1b Lazy-region alloc-under-lock — CLOSED by ownership (`mmap`/`mprotect` writers)

The `mmap`/`mprotect` writers still allocate on the heap while holding a
lazy-region lock with IRQs masked, but that is no longer rule-2 fuel, because
the lock is no longer *global*.

The map used to be `LAZY_REGION_TABLE: Spinlock<BTreeMap<Pid, BTreeMap<usize,
LazyRegion>>>` in `process/table.rs`. A `BTreeMap::insert` that OOM'd inside
`push_lazy_region_with_source` / `update_lazy_region_flags` routed through
`alloc_error_handler` → `return_to_kernel`, whose teardown called
`clear_lazy_regions` → `LAZY_REGION_TABLE.lock()` — re-entering the lock the
abandoned frame still held, and (since there is no unwinding) wedging that lock
for the rest of the boot. That was the `-j4` self-host freeze of 2026-08-02.

It now lives on the process: `Process::lazy_regions: Spinlock<LazyRegionMap>`
(fix direction (3), "structural (durable)", in the archive doc). Consequences:

- The lock is reachable only through a `Process`, and each process has its own,
  so a hang would need the dying thread's *own* map — and the teardown paths no
  longer touch it. `return_to_kernel`, `return_to_kernel_from_fault` and
  `teardown_forked_process_thread_group` dropped their `clear_lazy_regions`
  calls; the field is released by `Process::drop` on the existing reclaim path.
- `clear_lazy_regions` survives only for the `sys_wait4`/`sys_waitid` zombie
  reap, which runs in the *parent's* syscall context holding no lazy-region lock.
- `LazyRegionMap` (in `process/children.rs`) is the pure data structure and is
  unit-tested on the host; the pid-keyed free functions are thin wrappers that
  resolve `pid` → `Process` first.

Full audit, the deadlock chain, and why the tactical `try_lock`-and-skip fix
would *not* have been sufficient:
[`../../archive/LAZY_REGION_TABLE_ALLOC_UNDER_LOCK_HANG.md`](../../archive/LAZY_REGION_TABLE_ALLOC_UNDER_LOCK_HANG.md).

The general in-tree patterns for keeping allocation out of a lock hold, for the
sites that still share a global one: `syscall/fs.rs:318/353/398` (`drop(ts)`
before the copy), `syscall/poll.rs:334-339` (copy hoisted out of the lock),
`exceptions.rs:1180-1190` (pre-map then write).

### 5.2 Drain ambient-context gap

`drain_retired_under_pressure` fires from the fault path; its ambient lock set
is the faulting syscall's. Combined with §5.1 the drain can re-enter a held
lock. Mitigations under discussion: check the fixup handler before the lazy
resolver enters the pressure ladder, or force `alloc_page_zeroed` (no-drain)
when a `copy_*_safe` window is open.

### 5.3 Alloc-under-lock sites that route to the hijack leaves

`SHARED_L0_TABLE` insert (fork), `fds.table` clone in `close_all`,
`pipe_write` buffer growth, epoll/eventfd interest inserts, heap allocation
inside `with_irqs_disabled` process-table scans
(`process/mod.rs:1077,1226,1251,1302`). Any of these failing under heap pressure
becomes rule-2 hang fuel.

The lazy-region writers (`push_lazy_region*`, `update_lazy_region_flags`,
`munmap_lazy_region_overlapping`, `clone_lazy_regions`) used to belong on this
list and no longer do — see §5.1b for why owning the map per-process took them
out of rule-2's reach. That is the template for retiring the rest: give the
state an owner whose drop is already on the teardown path, rather than a global
the teardown has to reach back into.

## Verify

- Leak guard: boot devbox-smoltcp, run ≳50 `ssh … '<cmd>'` execs, confirm no
  `[HEAP-GROW]` 256 MB crossing and no `[OOM] allocation of … failed` lines.
- Hang shape triage: serial log **frozen** + 100 % CPU + no `[OOM]` line ⇒
  rule-2 spin (attach lldb to the gdbstub, the PC sits in a `Spinlock` spin
  loop); `[OOM] … killing process` repeating with live PSTATS ⇒ heap
  exhaustion, not yet the hang.

## Background

- `docs/archive/EXECVE_STACK_LEAK_OOM_HANG.md` — the 2026-08-02 investigation
  this doc distills (leak + hang chain, hammer repro, log forensics).
- `docs/archive/STALE_THREAD_SLOT_KILL.md` — the stale-slot kill race and the
  `unregister_process` backstop (§5.1 there: why PHASE 2 must not clear
  `thread_id`; its "the PHASE 2 edit hung the box" evidence is superseded —
  the hang matches the leak/hang chain above).
- `docs/archive/OOM_KILL_DEFERRED_RECLAIM_GAP.md` — why the pressure drain
  exists.
- `docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md` — the earlier
  self-deadlock that ruled out reclaim-from-`register_process`; the same
  argument governs §4 rule 2.
