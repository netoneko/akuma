# The identity cache at SMP>1 — two stale-`Process` windows

**Date:** 2026-08-27
**Scope:** follow-up item 1 of
[`AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`](AKUMA_SYSCALL_PERFORMANCE_AUDIT.md)
§"Resolution" — *"Not yet soaked at SMP>1 … run `forktest_smp_matrix` before
trusting SMP=4 numbers."*
**Status:** two defects found **by inspection**, not by reproduction. See
§"What the soak actually proves" before treating a green run as a clearance.

## Summary

The syscall audit (`c2a0e630`) took `getpid` from 410 ns to 150 ns by replacing
up to nine "who am I" resolutions per syscall with one cached identity. That
result stands and is not in question here.

But the nine resolutions were doing **two** jobs, and only one of them was
written down. Each `lookup_process_shared` was a *lookup* and — incidentally —
a *liveness check*: it returns `None` for a slot that is no longer ACTIVE, so
the old epilogue silently skipped its writes when the process had died mid
syscall. The rewrite kept the lookup and dropped the check in two places.

| | what re-validates | what is missing |
|---|---|---|
| A. `handle_syscall` epilogue | nothing — reuses the prologue pointer | any re-check after an open-ended dispatch |
| B. `table::identity_get` | `SLOT_STATES[slot] == ACTIVE` | ACTIVE cannot distinguish "still ours" from "freed and re-issued" |

Both are use-after-free classes, and both are **specifically amplified by
SMP>1** because every secondary core's idle loop is a reclaim drain site.

## Finding A — the epilogue writes through the prologue's `Process`

`src/syscall/mod.rs`, `handle_syscall`. The prologue resolves once:

```rust
let cur = akuma_exec::process::table::current_thread_tgid_process();
let owner_pid = cur.map_or(0, |(pid, _)| pid);
```

…and the epilogue, **after the dispatch**, reuses that same `&'static Process`:

```rust
if let Some((_, proc)) = cur { proc.current_syscall.store(!0u64, Relaxed); } // unconditional
if track_time && let Some((_, p)) = cur { p.syscall_stats.add_time_us(..); }  // default on
if logging { let box_id = cur.map_or(0, |(_, p)| p.box_id); .. }              // default on
```

`PROCESS_SYSCALL_STATS` and `PROC_SYSCALL_LOG_ENABLED` are both `true` except
under `kernel_profile_extreme` (`src/config.rs:655-677`), and the first write is
unconditional — so all three are live in the shipping devbox build.

Before the change the epilogue re-resolved (`read_current_pid` +
`lookup_process_shared(owner_pid)`), which returns `None` for a RETIRED slot.
**That lookup was the guard.**

The in-code justification —

> `cur` is the prologue's resolution, still valid for this excursion: the
> own-process lifetime guarantee is exactly what `lookup_process_shared`
> documents for this call shape

— does not hold, on two counts:

1. `cur` is the **tgid-leader** half, not the own half. The leader's `Process`
   belongs to another thread; the "own-thread lookup stays valid" reasoning
   never covered it.
2. `lookup_process_shared` documents validity only *"while the process stays
   registered"*, and the kernel deliberately unregisters processes whose threads
   are still running.

### The live trigger

`crates/akuma-exec/src/process/mod.rs`, `kill_thread_group`. PHASE 1 only
*requests* a deferred kill and grace-waits (up to 2 s) for each sibling to reach
its EL1→EL0 boundary — i.e. the sibling is still executing kernel code. Then,
per sibling:

```rust
let _ = table::unregister_process(*sib_pid);   // ACTIVE -> RETIRED, right now
...
crate::threading::get_waker_for_thread(*tid).wake();   // and only then wake it
```

The woken sibling unwinds its blocking syscall and returns **through the
epilogue**, holding a pointer to a `Process` that is already retired.

`unregister_process` does not clear the identity cache, but that is beside the
point here: `cur` is a stack-local copy, so no cache invalidation could have
helped. **Finding A is a hoisting bug, not a cache bug** — it would exist with a
perfect cache.

### Why SMP>1 is what makes it bite

Retire → free needs only `PROCESS_RECLAIM_COOLDOWN_US` = **10 ms**
(`src/config.rs:978`), and the collector is not just the 100 ms `netpoll_maint`
tick:

```rust
// src/smp_shared.rs:1051 — per-core idle loop
akuma_exec::process::reclaim::drain_retired_if_requested();
```

Every secondary core drains retired processes whenever it has nothing to run.
At SMP=4 (three secondaries confirmed online in the soak logs) an idle peer
frees the `Process` ~10 ms after retire. A woken-but-not-yet-scheduled sibling
delayed past that writes two atomics into a freed — and, `Process` being a
fixed-size allocation, very likely already reallocated — heap block.

The reclaim cooldown's own docs state the precondition this violates:

> `process_reclaim_cooldown_us` must outlast any such window; those windows are
> **single bounded I/O ops or PTE-chunk copies, not open-ended**.

A `ppoll`/`futex`/blocking-`read` dispatch is exactly the open-ended window that
sentence rules out.

### Fix

Re-resolve from the cache after the dispatch. The cache read re-validates slot
state, so a retired process yields `None` and the epilogue skips its writes —
identical to the pre-change behaviour, at ~2 loads instead of the lock + map
walk + table scan the old shape paid twice here.

## Finding B — `ACTIVE` does not survive slot recycling

`table::identity_get` validates the slot state and then dereferences the
**cached** pointer:

```rust
if slot >= MAX_PROCESSES || SLOT_STATES[slot].load(Relaxed) != ACTIVE { fallback }
let ptr = e.own_ptr/e.tgid_ptr.load(Acquire);
... Some((pid, unsafe { &*ptr }))
```

The slot lifecycle is ACTIVE → RETIRED → (`reclaim`: `Box::from_raw` + drop) →
FREE → **claimed again → ACTIVE**. After recycling the state reads ACTIVE while
the cached pointer is dangling, so the check passes and the deref is a UAF.

The `own` half is cleared by `thread_pid_map_remove`. The **`tgid` half is never
invalidated when the leader retires** — nothing clears a sibling's cached leader
pointer, and `kill_thread_group` removes the map entry only
`if owner == Some(*sib_pid)` and only `if let Some(tid) = sib_tid`.

### The live trigger

A `CLONE_THREAD` thread shares its address space, so on exit
(`process/mod.rs`, `return_to_kernel`):

```rust
if !is_shared && l0_phys != 0 { kill_thread_group(pid, l0_phys, exit_code); }
...
unregister_process(pid);
```

`is_shared` is true → the group is **not** torn down → only this thread's
`Process` is retired while its siblings keep running. When the exiting thread is
the **group leader**, every surviving sibling still caches `tgid_ptr` → the
leader's `Process`, which is freed 10 ms later and whose slot is then reissued.
From that point every sibling syscall dereferences it, because the prologue's
`read_current_pid()` goes through `current_thread_tgid_process()`.

### Why pointer-equality is not enough

Re-reading `PROCESS_SLOTS[slot]` and requiring it to equal the cached pointer
looks like the cheap fix, but `Process` is a fixed-size allocation: the
allocator can hand the *same address* to the new occupant. That passes the
pointer check while returning the wrong identity.

There are therefore two sound validations, and they are **alternatives, not a
pair**:

| | checks | cost | notes |
|---|---|---|---|
| generation | `state == ACTIVE && SLOT_GEN[slot] == stamp` | 2 loads | no `Process` deref |
| pointer + pid | `state == ACTIVE && PROCESS_SLOTS[slot] == ptr && (*ptr).pid == pid` | 3 loads | touches the `Process` line |

Under the generation scheme the pid check is **redundant**: `PROCESS_SLOTS[i]`
is written in exactly one place (`try_claim_free_slot`, after the CAS) and
nulled in one place (reclaim's swap), so within a single generation the slot's
pointer is immutable — a matching stamp already proves the slot holds the same
occupant, and the cached pid is correct by construction.

Without a generation the pid check becomes *mandatory*, and its ordering
matters: `PROCESS_SLOTS[slot] == ptr` must be confirmed **first**, because that
is what proves the pointer is the slot's current live occupant and so makes the
subsequent `(*ptr).pid` read safe.

The generation scheme is chosen here: it is cheaper, its safety argument is
shorter, and it keeps `read_current_pid` — which wants only the pid scalar — off
the `Process` cache line.

### Fix

A per-slot reuse generation:

```rust
static SLOT_GEN: [AtomicU32; MAX_PROCESSES];
```

bumped in `reclaim_retired_processes_internal` — the only site that frees —
between the pointer swap and the `FREE` store. The slot is RETIRED across that
bump, and readers already fall back on any non-ACTIVE state, so no reader can
observe ACTIVE paired with a stale stamp. The cache stamps the generation on
write and re-checks it on read. One extra load, no `Process` deref (so
`read_current_pid`, which only wants the pid scalar, stays off the `Process`
cache line).

## The soak harness was broken in four independent ways

The first full run reported **14 / 14 FAIL** — and every one of those failures
was the harness, not the kernel. This is recorded in detail because the failures
were individually plausible as kernel bugs, and three of the four produced
*false* failures (the direction that wastes the most time).

All four were confirmed by hand against a live SMP=2 devbox, not inferred:

| # | bug | symptom it manufactured |
|---|---|---|
| 1 | 5 configs passed no `-duration` | `[FAIL] Forktest timed out` on a healthy kernel |
| 2 | reader thread `return`ed at the boot marker | blind crash detection **and** VM stalls |
| 3 | readiness by console-marker match | `[FAIL] sshd did not start in time` at SMP=4 |
| 4 | no `BatchMode` on a pubkey-only server | would burn the timeout on an auth prompt |

**1. Unbounded runs.** `forktest_parent`'s `-duration` defaults to `0`, which
means *"run until all children finish"* (`userspace/forktest/parent/main.go:28`).
`basic`, `mmap_test`, `file_io`, `signal` and `goroutine_stress` all omitted it,
so they could never finish inside the `duration + 30` s subprocess timeout — a
guaranteed FAIL at every SMP level, which is exactly why SMP=2 and SMP=4 failed
identically. The two configs that *do* pass `-duration` work: run by hand,
`combined_light` returns `rc=0` in **14.2 s** against the 50 s allowed.

**2. The undrained pipe.** The reader thread did `return True` on the boot
marker, so nothing consumed QEMU's stdout afterwards. Two consequences: every
per-test log stopped at boot (so the "Kernel Diagnostics" grep for `[PANIC]` /
`WILD-DA` / `[SGI-S POISON]` / `[WATCHDOG]` could only ever match *boot*
output), and once the 64 KB pipe filled, QEMU blocked on write and the VM
froze — which is what turned a 14 s `combined_light` into a 50 s timeout.
It also closed the log under the still-running thread, giving
`ValueError: I/O operation on closed file`.

**3. Console-marker readiness is unsound at SMP>1 — but the console is fine.**
`CONSOLE_LOCK` is default-on in `release` (`build.rs:168`) and works: each
`console::emit` is atomic. The problem is that a *logical line* spans several
emits — `userspace/herd/src/main.rs:906` prints `"[herd] Started "` as its own
call, then the name, then the pid — so another core's emit lands in the gap:

```
[herd] Started [syscall] bind(fd=3, port=22, ip=0.0.0.0)
sshd (pid= 2)
```

No console lock can join separate `write(2)` calls, so **this is not a defect
to fix in the kernel** (the unlocked-UART tradeoff is deliberate). It means
readiness must be probed behaviourally. Observed 0/7 torn at SMP=2, 4/7 at
SMP=4.

**4. `connect()` is not readiness either.** The obvious replacement — poll TCP
2222 — is also wrong: QEMU's user-mode hostfwd accepts immediately, before the
guest listens (measured: "accepting after 1s" on a VM that had barely begun to
boot). Only reading `SSH-` off the socket proves sshd is serving.

The harness now bounds every config, drains until EOF, waits for a real SSH
banner, and passes `BatchMode=yes`.

### Reading a green run

Even repaired, **a PASS means "forktest exited 0 and no crash marker was
printed"**. Both findings above are narrow timing windows whose failure mode is
a silent write into a reallocated heap block, not a panic. A green matrix is
evidence the windows are narrow, not evidence they are closed — which is why
the next step is a counter that observes the window directly (below), not a
speculative fix.

## Background

- [`AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`](AKUMA_SYSCALL_PERFORMANCE_AUDIT.md) —
  the audit this follows up, its ablation ladder and deferred item list.
- [`BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md`](BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md)
  — why retirement is deferred and what the cooldown is sized against.
- [`STALE_THREAD_SLOT_KILL.md`](STALE_THREAD_SLOT_KILL.md) — the sibling
  thread-slot recycling rules `kill_thread_group` follows.
