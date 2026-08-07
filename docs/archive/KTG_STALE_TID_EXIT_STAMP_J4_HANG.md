# The `-j4` self-host hang: kill_thread_group's stale-tid exit stamp (Failure D root cause)

**Date:** 2026-08-07
**Build:** `release-smp-shared` SMP=4, in-guest `cargo build --release -p akuma -j4 --offline`
**Status:** root-caused live, fixed, regression-tested (`test_ktg_stale_tid_channel_not_stamped`)

## Symptom

A `-j4` self-host kernel build hangs forever: cargo prints
`warning: build failed, waiting for other jobs to finish...` and never exits.
A rustc child (compiling `quote`'s build script) sits idle. The futex table
shows only healthy timed-park loops (`hist=uSep…` — unpark, self-remove,
re-enqueue, park; no lost wakes, no orphans). The system is otherwise fully
responsive over SSH. Historically filed under "jobserver token loss".

## Live inspection (VM kept running ~33 min after the hang)

The decisive artifact was a **pipe refcount ledger** built from the serial
log's `[pipe] create/clone_ref/close_read/close_write` lines, cross-checked
against `/proc/<pid>/fd` in the live guest:

- rustc (pid 126) blocked in `read(fd 9)`; `/proc/126/fd` maps fd 9 → pipe 94.
- `[PIPE-DUMP]`: pipe 94 `bytes=0 readers=1 writers=1` — but a sweep of every
  process's `/proc/*/fd` found **no write-end fd anywhere in the system**.
  The write refcount was leaked; the blocked reader could never see EOF.
- Pipes 93/94 were the stdout/stderr of rustc's linker child (`gcc` pid 138 →
  `collect2` pid 139 → `ld` pid 140). The ledger balanced perfectly for pipe 93
  (5 clone_refs, 5 closes → 0) and was one close short for pipe 94.

The missing close belonged to `ld` (pid 140) — which **never exited**:

```
[KTG] my_pid=113 my_tgid=113 by_tid=18 code=0 siblings=2 ...        (T85.9)
[KTG-STALE] my_pid=113 sib_pid=117 tid=31 recycled to pid=Some(140) — not terminating   (T87.89)
[T87.89] [mmap] pid=140 ...                                          (ld still linking)
[TERM] tid=31 pid=Some(140) by_tid=26 state=1 ... at=table.rs:199    (T87.90, collect2 reaps LIVE ld)
[T87.90] [PROC-EXIT] pid=139 collect2 code=0
[T87.92] [PROC-EXIT] pid=138 gcc code=0
[pipe] close_write id=93 write_count=0 read_count=1                  (ld's teardown: ONE close)
[Cleanup] Thread 31 recycled after 31794us cooldown                  (…then nothing more)
```

No `[PROC-EXIT]` for pid 140, no self-`[TERM]`, no pipe-94/jobserver closes.

## Causal chain

1. **A concurrent job's thread group (pids 112/113/117) tears down.**
   `kill_thread_group(113)` snapshots `siblings = [(116, tid 30), (117, tid 31)]`.
   Sibling 117 was already dead; the snapshot records its stale `thread_id`
   (PHASE 2 deliberately leaves `thread_id` set on dead siblings).
2. **During the 2 s kill grace, slot tid 31 is recycled** to freshly spawned
   `ld` (pid 140). The grace-expiry hard kill correctly skips the recycled slot
   (`[KTG-STALE] … not terminating`).
3. **PHASE 2 had no such guard.** It ran `remove_channel(31)` and stamped
   `set_exited(0)` — group 113's exit code — onto the channel now registered by
   ld. The per-tid channel registry entry is the *same* `Arc<ProcessChannel>`
   the per-child-pid registry serves to `wait4`.
4. **collect2's `wait(ld)` instantly saw `has_exited() == true`, code 0** — a
   forged clean exit for a live, mid-link process. It reaped ld
   (`unregister_process` → `mark_thread_terminated(31)`, legitimate from its
   view) and exited "success"; gcc followed; rustc's ppoll saw pipe 93's EOF.
5. **ld's fd teardown was abandoned mid-sweep.** Its thread, already marked
   TERMINATED, ran `close_all()` and was descheduled after the first close
   (pipe 93); TERMINATED threads are never resumed, and the slot was recycled
   5 log lines later. Because `close_all()` used snapshot-then-clear, the
   unclosed entries (pipe 94 write end + two jobserver pipe refs) had already
   left the table — no later drop/backstop could ever find them.
6. **rustc blocked forever in `read()` on pipe 94** waiting for an EOF that
   could no longer arrive; cargo waited forever on rustc. That is the hang.

Every prior framing is explained: "live-but-idle rustc", "jobserver token
loss" (the leaked refs included jobserver pipe ends; tokens sat unread in the
pipe), "not a scheduler bug" (correct — futex/scheduler were healthy).

## Fixes (all in this change)

1. `kill_thread_group` PHASE 2: channel eviction + exit stamp now require
   `table::pid_for_thread(tid)` to be `None` or the sibling itself — the same
   ownership rule the map eviction below it and the grace-expiry hard kill
   already used. New `[KTG-STALE-CH]` tripwire when skipped.
2. `kill_thread_group` PHASE 1 (both smp-shared `request_thread_kill` and
   single-core `mark_thread_terminated`): same ownership guard, so kill flags
   are not planted on FREE/recycled slots.
3. `SharedFdTable::close_all()`: pops and closes ONE entry at a time instead
   of snapshot-then-clear. An abandoned sweep now leaves unclosed entries in
   the table where the `Drop` backstop (deferred process reclaim) closes them —
   damage bounded to the single in-flight entry.

Regression test: `test_ktg_stale_tid_channel_not_stamped` in
`src/process_tests.rs` (boot suite) — builds the exact scenario (dead sibling
with recorded tid recycled to a victim, plus a legitimately-owned sibling) and
asserts the victim's channel is untouched while the owned sibling still gets
the group exit code (the goroutine-leader case the stamp exists for).

## Implementation spec (the exact solution — matches the uncommitted working-tree diff of 2026-08-07)

Three files change. The diff already exists in the working tree on branch
`another-smp-attempt-0`; this section is the authoritative description of what
it must do, so it can be reviewed, re-derived, or re-applied independently.

### A. `crates/akuma-exec/src/process/mod.rs` — `kill_thread_group`

The single source of truth for "is this recorded tid still the sibling's" is
the existing predicate (already used by the grace-expiry hard kill and by
`unregister_process`):

```rust
pub fn grace_kill_should_terminate(sib_pid: Pid, tid: usize) -> bool {
    table::pid_for_thread(tid) == Some(sib_pid)   // resolves THREAD_PID_MAP
}
```

**A1. PHASE 1, `#[cfg(kernel_smp_shared)]` branch** — the deferred-kill
request loop. Before: `request_thread_kill(*tid)` unconditionally for every
recorded sibling tid. After:

```rust
for (sib_pid, sib_tid) in &siblings {
    if let Some(tid) = sib_tid {
        if grace_kill_should_terminate(*sib_pid, *tid) {
            crate::threading::request_thread_kill(*tid);
        }
    }
}
```

Rationale: a sibling that died before the group kill keeps its recorded
`thread_id` in the snapshot. Posting a kill to a FREE slot plants a
`PENDING_KILL` flag its next claimant may inherit; posting to a recycled slot
aims at an innocent thread. `None` (FREE) is deliberately **skip** here —
kills follow the strict-ownership rule the grace path already chose.

**A2. PHASE 1, `#[cfg(not(kernel_smp_shared))]` branch** — the direct
`mark_thread_terminated(*tid)` loop gets the identical guard. Single-core is
not exempt: the staleness exists at snapshot time (the dead-sibling case),
not only from cross-core concurrency.

**A3. PHASE 2 — the channel eviction + exit stamp.** This is the load-bearing
fix. Before:

```rust
if let Some(tid) = sib_tid {
    if let Some(channel) = remove_channel(*tid) {
        if !channel.has_exited() { channel.set_exited(exit_code); }
    }
}
```

After:

```rust
if let Some(tid) = sib_tid {
    let owner = table::pid_for_thread(*tid);
    if owner.is_none_or(|o| o == *sib_pid) {
        if let Some(channel) = remove_channel(*tid) {
            if !channel.has_exited() { channel.set_exited(exit_code); }
        }
    } else if KTG_STALE_CH_SKIPS.fetch_add(1, Ordering::Relaxed) < 64 {
        // rate-limited "[KTG-STALE-CH] my_pid=… sib_pid=… tid=… recycled to
        // pid=… — not stamping channel" print, same style as [KTG-STALE]
    }
}
```

Two semantic decisions, both deliberate and DIFFERENT from the kill guards:

- **`None` (FREE slot) → proceed.** A dead thread's leftover per-tid channel
  entry is exactly the garbage this eviction exists to collect, and the
  `!channel.has_exited()` check already protects a channel that recorded a
  real exit code from being clobbered. Only `Some(other_pid)` — a live
  recycled owner — must be left alone.
- **`Some(sib_pid)` → stamp.** The stamp must keep working for a sibling that
  legitimately owns its slot: when a goroutine calls `exit_group(0)`, the
  group *leader* is one of these siblings and its channel is what the shell's
  `wait4` reads — this is why the stamp exists at all (see the pre-existing
  comment about the hardcoded `-9` regression).

New statics next to `KTG_STALE_SKIPS`:

```rust
/// NOT cfg-gated: PHASE 2 runs in every build, and the snapshot can be stale
/// on entry (a sibling that died before the group kill keeps its recorded tid).
static KTG_STALE_CH_SKIPS: AtomicUsize = AtomicUsize::new(0);
```

Note the asymmetry with `KTG_STALE_SKIPS`, which is `#[cfg(kernel_smp_shared)]`
because only the grace-wait exists there. Do not gate the new one.

### B. `crates/akuma-exec/src/process/fd.rs` — `SharedFdTable::close_all`

Before (the leak enabler): snapshot `table.values().cloned()` + `table.clear()`
under one lock hold, then close from the local `Vec` outside the lock. An
abandoned sweep loses every not-yet-closed entry irrecoverably, because they
already left the table.

After: pop-per-entry —

```rust
pub fn close_all(&self) {
    loop {
        let entry = with_irqs_disabled(|| self.table.lock().pop_first());
        let Some((_fd, fd)) = entry else { break };
        match fd { /* unchanged per-variant close arms */ }
    }
}
```

Properties that must hold: each entry is closed at most once even with
concurrent `close_all` callers (pop is atomic under the lock); closes still
run WITHOUT the table lock held (`pipe_close_write` takes `PIPES` and can
re-enter teardown paths); idempotent (`Drop for SharedFdTable` calls it
again); an abandoned sweep leaves the remaining entries in the table where the
`Drop` backstop (deferred process reclaim) closes them later — worst case, one
in-flight entry's close is lost instead of the whole tail.

### C. `src/process_tests.rs` — boot-suite regression test

`test_ktg_stale_tid_channel_not_stamped`, registered in the runner right after
`test_kill_thread_group_two_phase()`. Construction constraints that matter:

- Fake tids must be `MAX_THREADS + n` (NOT 200/210-style constants — those are
  in-range on the 256-slot default profile). Out-of-range tids make
  `mark_thread_terminated` / `request_thread_kill` bounds-checked no-ops while
  `THREAD_PID_MAP` and the channel registry (plain BTreeMaps) behave for real.
- `is_thread_terminated(out_of_range) == true` (state reads FREE), which is
  what keeps the smp-shared grace-wait loop from stalling 2 s in the suite.
- Scenario: leader (tgid = own pid, `thread_id = None`) + dead sibling
  (`tgid = leader`, `thread_id = Some(stale_tid)`,
  `register_thread_pid(stale_tid, victim_pid)` where victim is never
  registered as a process) + owned sibling (`thread_id = Some(owned_tid)`,
  `register_thread_pid(owned_tid, live_sib_pid)`). Register a fresh
  `ProcessChannel` under each tid, call `kill_thread_group(leader, l0, 0)`.
- Assert: victim's channel `!has_exited()` AND still present via
  `get_channel(stale_tid)`; owned sibling's channel `has_exited()` with
  `exit_code() == 0` AND evicted (`get_channel(owned_tid).is_none()`).
- Cleanup: `remove_channel(stale_tid)`, `unregister_thread_pid` both tids,
  `clear_lazy_regions` + `unregister_process` the leader,
  `reclaim_retired_processes_force()`.

### What is intentionally NOT part of this solution

- No generation counters on the tid/channel registries (the rigorous TOCTOU
  fix — see Residual risks). The guards shrink the window from "2 s grace"
  to "µs between check and act"; they do not close it.
- No change to `wait4`'s trust in `has_exited()`.
- No change to PHASE 2's `cleanup_process_fds(sib)` call — it resolves the
  sibling's own `Process` by pid, not by tid, and is not staleness-exposed.

## Residual risks / not fixed here

- **TOCTOU:** every ownership check is check-then-act; a slot can recycle
  between the check and the action. The rigorous fix is a generation-carrying
  channel/tid registry (WakeHandle-style). Window is ~µs vs the ~ms recycle
  cooldown.
- **Abandoned-execution class:** a thread marked TERMINATED while running its
  own kernel teardown still abandons whatever it was doing at the next
  preemption; `close_all` is now tolerant, but other teardown steps
  (`remove_child_channel`, epoll destroy, …) run at most once per entry and a
  mid-`match` abandonment still loses that single entry's side effects.
- **`wait4` trusts channels absolutely.** A second forged-exit source would
  produce the same reap-of-live-child. A cheap tripwire would be `wait4`
  cross-checking `thread_for_pid(child)` liveness before reaping.
- The pre-existing boot-suite failure `thread_slot_reclaim_on_spawn`
  (`hot_reclaim=45–48, want 0`) reproduces identically with and without this
  change (A/B-verified 2026-08-07); unrelated.

## Verify

- Boot suite: `[Test] ktg_stale_tid_channel_not_stamped PASSED`, and the
  neighboring `kill_thread_group_two_phase` / `external_kill_closes_shared_fds`
  still pass.
- Under a `-j4` self-host build, grep the serial log for `[KTG-STALE-CH]` — each
  hit is a forged exit prevented. A hang of this class would additionally show
  a `[PIPE-DUMP]` pipe with `writers>0` that no `/proc/*/fd` entry references.

## Background

- `docs/archive/J4_HANG_LIVE_AUTOPSY.md` — the full session record: evidence trail in discovery order, decoded diagnostics, ruled-out dead ends, reusable technique notes
- `docs/archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md` (§7.15–7.17: the elr diagnostic, write-position race, clock_nanosleep — earlier layers of the same campaign)
- `docs/archive/STALE_THREAD_SLOT_KILL.md` (the ownership rule's origin)
- `docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md` (deferred reclaim; why the `Drop` backstop exists)
