# Dead-code sweep: findings to fix (2026-08-07)

Actionable output of the dead-code measurement recorded in
[`LINE_COUNT_ANALYSIS.md`](LINE_COUNT_ANALYSIS.md) §"Stat 5". That doc has the
statistics and their interpretation; this one is the list of real defects the
sweep surfaced, ranked by severity, with the evidence chain for each.

**Status: nothing here is fixed. No source was edited.** Every claim below is
read from the code at commit `d3f28d6` (branch `another-smp-attempt-0`) — none of
it was observed at runtime, and nothing was booted to confirm. Verification steps
are given per finding.

How the sweep was run (`dead_code` is `deny` workspace-wide, so everything dead
sits behind one of 76 explicit `#[allow(dead_code)]`):

```bash
export CARGO_TARGET_DIR=/tmp/dc-target       # keep other sessions' cache intact
export RUSTFLAGS="--force-warn dead_code"    # overrides the allow attributes
cargo check --message-format=short 2>&1 | grep -E 'never (used|read|constructed)'
```

---

## 1. Message queues survive box teardown, and box ids are reused

**The only finding here that is a live defect rather than untested or unused
code.**

### Evidence

`cleanup_box_queues` exists, documents its own caller, and has none:

```rust
// src/syscall/msgqueue.rs:434
/// Called from sys_kill_box to remove all queues belonging to a box.
#[allow(dead_code)]
pub fn cleanup_box_queues(box_id: u64) {
    crate::irq::with_irqs_disabled(|| {
        let mut table = MSGQUEUE_TABLE.lock();
        table.retain(|(bid, _), _| *bid != box_id);
    });
}
```

`grep -rn cleanup_box_queues src crates` returns only the definition. And
`sys_kill_box` does exactly two things, neither of them this:

```rust
// src/syscall/container.rs:36
pub(super) fn sys_kill_box(box_id: u64) -> u64 {
    crate::vfs::remove_box_namespace(box_id);
    if akuma_exec::process::kill_box(box_id).is_ok() { 0 } else { ESRCH }
}
```

Note the asymmetry: the VFS namespace *is* torn down on the same path. IPC is the
one that isn't.

`MSGQUEUE_TABLE` is keyed `(box_id, msqid)` (`src/syscall/msgqueue.rs:58`), so
entries for a dead box remain addressable by that id, holding their queued
`VecDeque` payloads.

**Box ids are reused deterministically.** The kernel never allocates one —
`sys_register_box(id, …)` (`src/syscall/container.rs:4`) takes the id from
userspace, and herd derives it from a hash of the box *name*:

```rust
// userspace/herd/src/main.rs:881
fn generate_box_id(name: &str) -> u64 {
    let mut box_id = 0u64;
    for b in name.as_bytes() {
        box_id = box_id.wrapping_mul(31).wrapping_add(*b as u64);
    }
    if box_id == 0 { box_id = 1; }
    box_id
}
```

Same name → same id, every time. So id reuse across a box restart is guaranteed,
not merely possible.

### Impact

1. **Unbounded leak.** Every box create/kill cycle that used a message queue
   leaves the queue and its messages in `MSGQUEUE_TABLE` forever. Only an explicit
   `IPC_RMID` from a cooperating process frees one; a killed box never gets to
   send it.
2. **Cross-generation state bleed.** Restart a box under the same name and it
   inherits the previous generation's queues, including undelivered messages. This
   is the same class of bug as the isolation property that *is* tested — but
   `test_msgqueue_box_isolation` covers isolation between two *different* boxes,
   not between two generations of the same box, so nothing catches it.

Reachability: `sc-sysv-ipc` is in `default`, so this is live in `release`,
`release-smp-shared`, and both devbox images. msgqueue is currently the only SysV
IPC family implemented (no `SEMGET`/`SHMGET` in `src/syscall/mod.rs`), so no
sibling table has the same gap.

### Fix

One call in `sys_kill_box`, before or after `remove_box_namespace`:

```rust
crate::syscall::msgqueue::cleanup_box_queues(box_id);
```

Then drop the now-unneeded `#[allow(dead_code)]` at `src/syscall/msgqueue.rs:435`.
Consider whether waiters blocked on a queue belonging to the killed box need
waking before the entry is dropped — `cleanup_box_queues` currently discards the
`recv_pollers`/`send_pollers` maps without waking anyone, which would leave a
cross-box waiter blocked forever. (Whether a *foreign*-box thread can be parked on
another box's queue is unverified; if it can't, this is moot.)

### Verify

Boot-suite self-test in `src/process_tests.rs` (per the project rule that kernel
changes need one):

1. Register box `N`, create a queue in it, send a message.
2. `sys_kill_box(N)`.
3. Assert `msgqueue_message_count(N, msqid) == 0` and that the table has no entry
   for `N` — `msgqueue_message_count` is one of the accessors in §2, already
   present for exactly this purpose.
4. Re-register box `N` (same name → same id), assert `msgget` returns a queue with
   zero messages rather than the previous generation's.

---

## 2. Five msgqueue waker tests are disabled, so the wake path is untested

Not forgotten — deliberately disabled, with the reason recorded in place:

```rust
// src/process_tests.rs:570
// Message queue waker tests
// DISABLED: These tests manipulate real thread slots which causes scheduler crashes.
// They set threads to WAITING/READY states without proper context, and when the
// scheduler tries to switch to them, it crashes because sp=0.
// TODO: Rework these tests to use mock thread IDs >= MAX_THREADS.
// test_msgqueue_send_wakes_receiver();
// test_msgqueue_recv_wakes_sender();
// test_msgqueue_rmid_wakes_pollers();
// test_msgqueue_nowait_returns_immediately();
// test_msgqueue_waker_idempotent();
```

### What is and isn't covered

Of 8 msgqueue tests, **3 run** (`src/process_tests.rs:247-249`):
`test_msgqueue_create_destroy`, `test_msgqueue_send_recv`,
`test_msgqueue_box_isolation`. So create/destroy, send/receive and cross-box
isolation are covered.

**Untested: the waker layer** — that a blocked receiver is woken by a send, that a
blocked sender is woken by a receive, that `IPC_RMID` wakes everyone parked on the
queue, `IPC_NOWAIT` semantics, and waker idempotence. That layer is live in
production:

| site | what it does |
|---|---|
| `src/syscall/msgqueue.rs:190` | `sys_msgsnd` registers the blocking sender: `q.send_pollers.insert(tid, wake_handle_for_thread(tid))` |
| `src/syscall/msgqueue.rs:250` | `sys_msgrcv` registers the blocking receiver |
| `src/syscall/msgqueue.rs:197,274,289` | the counterpart drains and wakes the other side's handles |
| `src/syscall/msgqueue.rs:88-99` | `sys_msgctl` (RMID) wakes all pollers |

Given the project's history of scheduler/waker races (stale `ThreadWaker` reviving
recycled slots, the `ON_CPU` gate), an untested wake path in a default-enabled
syscall family is worth more than its 277 lines suggest.

### Why 9 functions show up dead alongside it

`msgqueue_add_recv_poller`, `msgqueue_add_send_poller`,
`msgqueue_recv_pollers_count`, `msgqueue_send_pollers_count`,
`msgqueue_is_recv_poller`, `msgqueue_push_direct`, `msgqueue_pop_direct`,
`msgqueue_message_count` (`src/syscall/msgqueue.rs:305-431`) are **external test
seams** — they let a test inspect and manipulate poller state without going
through the syscalls. They are dead *because* the tests are off, not because the
feature is unwired. Keep them; §1's verify step already needs one.

(`cleanup_box_queues` is in the same dead list but is **not** a test seam — see
§1.)

### Fix

Follow the existing TODO: rework the five tests to use mock thread ids
`>= MAX_THREADS` so they never hand a real slot to the scheduler, then uncomment.
Do not simply re-enable them — the recorded failure mode is a scheduler crash
(`sp=0`), which will take the boot suite down with it.

---

## 3. Six allocator pattern tests have zero call sites

296 lines of tests that nothing invokes — not even indirectly:

| test | lines | callers |
|---|---|---|
| `src/tests.rs:1782 test_resize_pattern` | 74 | 0 |
| `src/tests.rs:1723 test_memory_pool_pattern` | 55 | 0 |
| `src/tests.rs:1902 test_linked_structure` | 51 | 0 |
| `src/tests.rs:1637 test_lifo_pattern` | 39 | 0 |
| `src/tests.rs:1680 test_fifo_pattern` | 39 | 0 |
| `src/tests.rs:1860 test_temporary_buffers` | 38 | 0 |

Unlike §2 there is no comment explaining why, and unlike §2 they are not reachable
from any disabled aggregate either — `run_all` (`src/tests.rs:492`) calls only
`run_memory_tests`, `run_threading_tests` and `run_benchmarks`, none of which call
these six.

The rest of `src/tests.rs` does run: `src/main.rs:1007` calls
`tests::run_memory_tests()`, `:1062` `tests::run_threading_tests()`, `:1099`
`tests::run_benchmarks()`. So the file is not orphaned — these six tests are.

### Fix

A decision, not a patch: wire them into `run_memory_tests` (they are allocator
workload patterns, which is what that suite is for) or delete them. They exercise
heap patterns — LIFO/FIFO churn, pool reuse, realloc growth, transient buffers,
linked structures — that the allocator work (`talc` spans, heap-growth backoff,
`reclaim_to_pmm`) has repeatedly disturbed, so wiring is probably the better call.
Whichever way: they should not sit in the tree counting as coverage while never
running.

## 4. `test_forktest_parent_mmap` is disabled on runtime cost

`src/process_tests.rs:4636` (52 lines), commented out at its call site with a
stated reason:

```rust
// src/process_tests.rs:627
// test_forktest_parent_mmap(); // disabled: runs for up to 60s
```

So this is a documented tradeoff, not a drop — the boot suite runs on every boot
and a 60 s test is a real cost. But mmap-under-fork has its own bug history (the
CoW TLB/ASID flush fault, the `mmap_regions` race under `CLONE_VM`), so "never
runs" is the wrong resting state. Options: gate it behind an opt-in env knob or a
slow-test feature so it can be run deliberately, or shrink it until it fits the
per-boot budget.

Of the 7 disabled/orphaned tests, this and the five in §2 carry documented
reasons. The six in §3 are the only ones that were silently dropped.

## 5. `tests::run_all` is a redundant wrapper

`src/tests.rs:492`. `main.rs` calls the three constituent suites directly, so this
aggregate has no caller. Harmless; delete it, or keep it as the documented
convenience entry (`src/tests.rs:3` still tells readers to use it, which is
misleading as written — that line should point at the three real entry points).

---

## 6. Adjacent: `register_box` / `kill_box` skip the file's own authorization pattern

**Not a dead-code finding** — noticed while tracing §1, recorded here because it
is the same code path and changes how §1's id-reuse matters.

`src/syscall/container.rs` already has a consistent cross-box authorization rule:
an operation that reaches into another box's state requires the caller to be in
box 0.

```rust
// src/syscall/container.rs:108  sys_mount_in_ns
let caller_box = akuma_exec::process::current_process_shared().map_or(0, |p| p.box_id);
if caller_box != 0 { return EPERM; }
```

`sys_mount` (`:72`) and `sys_umount2` (`:95`) apply the same `box_id == 0` test.
The two syscalls that create and destroy box *identity* do not:

| syscall | nr | check |
|---|---|---|
| `sys_register_box` (`:4`) | 316 | user-pointer validation only — **no caller check** |
| `sys_kill_box` (`:36`) | 317 | none — **no caller check** |

Consequences, given that ids are userspace-supplied and name-derived (§1):

- A process inside box *N* can `register_box` with **any** id, including one
  already owned by another box — overwriting its `BoxInfo` and re-creating its VFS
  namespace. Ids are predictable by construction (hash of the box name), so
  guessing is not required.
- A process inside box *N* can `kill_box(M)` for any *M*, terminating another
  box's processes. Cross-box denial of service, in a kernel that gates *mounting*
  across the same boundary.

There is no uid concept in these paths — the existing checks are box-based, not
user-based — so this is a box→box boundary question, not a user→user one.

### Whether this is a security defect depends on a threat model that isn't written down

`docs/reference/subsystems/containers.md` describes boxes as an "isolation model"
and grades itself **B (watch)**, but states no threat model: nothing says whether
a box is expected to hold against hostile code inside it, or is only a
robustness/routing boundary for herd-supervised services. Today every process runs
as root over in-kernel SSH, there is no second principal, and the practical
function of a box is network-stack routing plus a VFS namespace — so nothing is
currently *escalating* across these gaps.

### Fix

Two parts, in order:

1. **Write the threat model into `containers.md`** — even one sentence ("boxes are
   a routing and namespacing boundary, not a security boundary; all processes are
   trusted") is worth more than the code change, because it stops the word
   "isolation" from being relied on for something it doesn't provide.
2. **If boxes are meant to become a security boundary**, then: gate
   `register_box`/`kill_box` with the same `caller_box != 0 → EPERM` rule the mount
   syscalls use; reject `register_box` for an id that is already registered to a
   live box; and allocate ids in the kernel rather than accepting them from
   userspace (which also removes §1's deterministic-reuse hazard). None of this is
   worth doing before (1) — the threat model decides whether kernel-allocated ids
   are a requirement or just tidier.

---

## No action needed

Recorded so a future sweep doesn't re-litigate them:

- **`src/console.rs` (10 items)** — the whole UART *input* path (`has_char`,
  `getchar`, `getchar_blocking`, `read_line`, `read`/`flags`/`has_data`,
  `FR_OFFSET`/`RXFE`/`TXFF`/`BUFFER_SIZE`). SSH is the console; serial input is
  output-only by design. Keep for bring-up debugging.
- **`src/config.rs` (12 constants under `default`, 17 under the `size` set)** —
  documented knobs whose consumers are compiled out or were removed. Cheap to
  audit, no correctness impact.
- **`crates/akuma-ext2` `hold` fields ×2** (`ext2.rs:607,622`) — RAII guards,
  where never-being-read is the entire point. Lint false positives.
- **12 items dead only under the `size` feature set** —
  `futex_wait_at_tgid_for_test`, `spurious_svc_count`, `DISABLE_ALL_TESTS`,
  `RUN_NETWORK_TESTS`, … Test hooks orphaned by `no-tests`; live in the default
  build.
- **6 items in `crates/akuma-exec`** — `thread_start`, `mark_thread_running`,
  `get_wake_time`, `free_stack_for_slot`, `execute_boxed`, `KERNEL_PHYS_BASE`.
  Not investigated individually; `free_stack_for_slot` and `execute_boxed` are the
  two worth a look, since a dead stack-free path is the shape of a leak.

Sweep coverage limit: `pub` items in `crates/` are exempt from `dead_code` (rustc
assumes public API is used), so the crates are under-reported — the 6 above are
only the non-`pub` ones. And this is compile-time reachability, not runtime
coverage; code that is reachable but never executes needs instrumentation and a
boot to find.

---

## Background

- [`LINE_COUNT_ANALYSIS.md`](LINE_COUNT_ANALYSIS.md) — the statistics this came
  from, and what they do and don't mean.
- `docs/reference/subsystems/syscalls/` — per-family syscall reference (SysV IPC).
- `docs/reference/subsystems/containers.md` — box lifecycle, which §1 is a gap in.
