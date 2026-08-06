# The grace-expired hard kill: `kill_thread_group` terminates threads it does not own — 2026-08-06

**Status**: Root-caused, fixed and A/B-verified 2026-08-06. The fix is
`grace_kill_should_terminate` (`crates/akuma-exec/src/process/mod.rs`), applied in
PHASE 1's grace-expiry branch, plus an ownership guard on PHASE 2's
`THREAD_PID_MAP` eviction. Regress with
`process::grace_kill_tests::grace_kill_forces_a_real_straggler_but_spares_recycled_and_quiet_slots`
(host test, `cargo test -p akuma-exec`).

**This is the third recurrence of one class.** `unregister_process` was hardened
against it in [`STALE_THREAD_SLOT_KILL.md`](STALE_THREAD_SLOT_KILL.md) §5, and
`entry_point_trampoline` in
[`TRAMPOLINE_STALE_PROCESS_RELR.md`](TRAMPOLINE_STALE_PROCESS_RELR.md), both by
routing through `THREAD_PID_MAP` instead of `Process::thread_id`. The grace-expiry
branch was simply never visited. §5 below argues the class will keep returning until
the field stops being directly actionable.

---

## 1. The symptom

A `-j4` self-host build runs crash-free — no `SIGSEGV`, no `[Fault]`, no `[RELR]` —
and then stalls with waiters parked in untimed `FUTEX_WAIT` for hundreds of seconds
while QEMU sits at ~3 % CPU. Every futex-side check passes: the waiters are correctly
queued at correctly address-space-scoped keys, there are no `[FUTEX-ORPHAN]` lines,
and the key namespaces are right.

The futex layer is behaving. What is stuck is a **process with no thread**.

## 2. The evidence

Two tracers, added for this investigation, made it visible in one run:

`[PROC-ORPHAN]` (printed next to `[THR-DUMP]`) walks every ACTIVE, un-exited process
and reports those with no live thread in `THREAD_PID_MAP`:

```
[PROC-ORPHAN] pid=92  tgid=14 no live thread; recorded thread_id=Some(23) now owned by pid=Some(103)
[PROC-ORPHAN] pid=57  tgid=14 no live thread; recorded thread_id=Some(28) now owned by pid=None
[PROC-ORPHAN] pid=115 tgid=14 no live thread; recorded thread_id=Some(18) now owned by pid=Some(337)
```

`tgid=14` is **cargo**. These are cargo's own worker threads, and they never come back
— the same three pids repeat in every 30 s dump for the rest of the boot.

`[TERM]` (`#[track_caller]` on `mark_thread_terminated`) names the site that killed
each one, with the owner resolved through `THREAD_PID_MAP` rather than the table scan:

```
[TERM] tid=23 pid=Some(92) by_tid=17 state=5 pending_kill=false at=crates/akuma-exec/src/process/mod.rs:1225
[TERM] tid=28 pid=Some(57) by_tid=17 state=5 pending_kill=false at=crates/akuma-exec/src/process/mod.rs:1225
[TERM] tid=17 pid=None     by_tid=20 state=0 pending_kill=false at=crates/akuma-exec/src/process/mod.rs:1225
```

Read the fields:

- **`by_tid` ≠ the subject** — a cross-thread kill.
- **`state=5`** (WAITING) — the victim was parked, minding its own business.
  `state=0` on the third line is FREE: that call marked an *empty slot* TERMINATED.
- **`pending_kill=false`** — the victim had no deferred-kill request. By the grace
  loop's own definition (`stragglers` counts only `has_pending_kill`) it was **not a
  straggler**, yet it was hard-terminated as one.
- The site is the grace-expiry branch of `kill_thread_group` PHASE 1.

Across the pre-fix run: **261 hard kills from that line, 179 of them (69 %) with
`pending_kill=false`.**

## 3. Mechanism

`kill_thread_group` PHASE 1 under `kernel_smp_shared` does not hard-kill. It posts
`request_thread_kill` to each sibling and grace-waits up to `KILL_GRACE_US` (2 s) for
them to self-terminate at their EL1→EL0 boundary. If the grace expires it forces the
issue:

```rust
for (_, t) in &siblings {
    if let Some(tid) = t {
        crate::threading::mark_thread_terminated(*tid);   // ← every recorded tid, unconditionally
    }
}
```

Two independent things are wrong with that loop.

**It ignores its own straggler test.** The line above logs a `stragglers` count
computed from `has_pending_kill`, then terminates every sibling regardless. A sibling
with no pending request either already consumed it (it is self-terminating at its
boundary) or never had one armed — neither warrants force.

**It acts on a two-second-old snapshot.** `siblings` is captured before the wait, and
holds `Process::thread_id` — a *recorded* slot number. The thread-slot recycle
cooldown is ~10 ms. Over a 2 s window a slot can be freed and re-claimed many times,
so the recorded tid routinely names a thread belonging to an unrelated process.

The two overlap, which is why the first is the load-bearing half in practice: the
recycler clears `PENDING_KILL` when it frees a slot, so a recycled slot almost always
fails the straggler test first. (The ownership check is still required for a slot
recycled *and* re-armed inside the window — and it is the check that states the
invariant, so it stays.)

### 3.1 Why the victim's process hangs forever

The killed thread belongs to a process that was **not** in `siblings`, so PHASE 2
never unregisters it. It is left ACTIVE, `exited == false`, with no thread:

- nothing can run in it, so it can never reach `exit_group`;
- so it never publishes an exit code;
- so it is never reaped;
- so its parent's `wait4` never returns.

The parent is the one that *looks* stuck, and it looks stuck in a futex, which is why
this class is repeatedly mistaken for a lost wakeup. The waiter is correctly queued;
it is waiting on a process that cannot move.

## 4. Two diagnostic false negatives this exposed

Both were actively misleading, and both are fixed:

**`[kill]` lines cannot rule out an external kill under `smp-shared`.** The tracer in
`mark_thread_terminated` fires only when `killer != idx`. The whole deferred-kill path
has the victim self-mark at its own boundary, so `killer == idx` and a thread killed
from outside dies with no trace. `debug-futex-lost-wakeup.md` §4a used "zero `[kill]`
lines in the entire boot ⇒ nothing was killed" as a *diagnosis*. Under `smp-shared`
that inference does not hold. The `[TERM]` tracer now attributes every termination.

**`[THR-DUMP]`'s `pid=` column is stale-prone.** It resolves via
`find_pid_by_thread`, which is the same `p.thread_id == Some(tid)` table scan the
trampoline bug was about — first ACTIVE match wins, so a stale process at a lower slot
index captures the attribution. The futex tables are trustworthy here and the thread
dump is not: futex keys record `tgid` at enqueue time via `read_current_pid`, which
resolves through `THREAD_PID_MAP`.

## 5. Why this class keeps coming back

`Process::thread_id` is a plain `Option<usize>` that any teardown path can read and
act on, and acting on it is wrong in exactly one circumstance that is invisible at the
call site: the slot has been recycled. Each fix so far has patched one caller:

| path | resolves through | hardened |
|---|---|---|
| `unregister_process` | `THREAD_PID_MAP` | 2026-08-02 (`STALE_THREAD_SLOT_KILL.md` §5) |
| `entry_point_trampoline` | `THREAD_PID_MAP` | 2026-08-06 (`TRAMPOLINE_STALE_PROCESS_RELR.md`) |
| `kill_thread_group` PHASE 1 grace expiry | `THREAD_PID_MAP` | 2026-08-06 (this doc) |
| `kill_thread_group` PHASE 2 map eviction | `THREAD_PID_MAP` | 2026-08-06 (this doc) |
| `kill_process`, `kill_fork_subtree_recursive` | clears the field first | — |

`STALE_THREAD_SLOT_KILL.md` §3.2's inventory of teardown paths lists PHASE 2 but not
the grace-expiry branch inside PHASE 1, which is how it survived the 2026-08-02 sweep.

The durable fix is to stop exposing an actionable bare slot number: make every
*acting* use go through `table::thread_for_pid` / `table::pid_for_thread`, and keep
`thread_id` as a diagnostic record only. That refactor was **not** done here.

## 6. The fix

```rust
pub fn grace_kill_should_terminate(sib_pid: Pid, tid: usize) -> bool {
    crate::threading::has_pending_kill(tid) && table::pid_for_thread(tid) == Some(sib_pid)
}
```

Also guarded: PHASE 2's `THREAD_PID_MAP.remove(tid)`. Evicting a recycled slot's entry
makes its new occupant's identity unresolvable, and `read_current_pid` returning None
silently degrades that thread's futex keys into the VA-only `tgid=0` namespace where
every process running the same binary collides — a lost-wakeup generator in its own
right. The `wake()` beside it is deliberately left unconditional: a spurious wake is
harmless (every park loop re-checks and re-parks) while a missed one is not.

## 7. Verification

A/B on identical disk clones (re-cloned from pristine between arms), same kernel
except the fix, same `-j4` cold self-host build, `SMP=4`, 4 GB:

| | pre-fix | fixed |
|---|---|---|
| rustc invocations (workload normaliser) | 81 | 75 |
| grace-expired hard kills | **261** | **0** |
| …with `pending_kill=false` (non-stragglers) | **179** | 0 |
| `[KTG-STALE]` — kills refused, slot recycled | — | **9** |
| distinct `[PROC-ORPHAN]` processes | **3** | **0** |
| SIGSEGV / `[RELR]` | 0 / 0 | 0 / 0 |

The nine `[KTG-STALE]` lines are what makes this conclusive rather than a quiet run:
each one is a hard kill that *was* aimed at a slot whose owner had changed, refused at
the guard. On the pre-fix kernel each would have terminated an unrelated process's
thread.

The pre-fix arm leaked its **first permanent orphan after 11 rustc invocations / 57
process exits** and never recovered — the same three pids repeat in every dump for the
rest of the boot. The fixed arm ran a comparable workload with none.

An earlier fixed-arm run (before the `wake()` was restored to unconditional, §6) is
kept as `ARM_FIXED_run1.log`: 48 rustc invocations, 0 orphans, 0 non-straggler kills,
but only 47 grace-expired kills and 0 `[KTG-STALE]` — the ownership guard never got a
chance to fire there because the recycler clears `PENDING_KILL`, so the straggler test
rejected those slots first. That is the ordering described in §3.

The host test is confirmed two-sided: reverting `grace_kill_should_terminate` to the
pre-fix `true` fails it on the recycled-slot assertion.

## 8. What this does not close

**The `-j4` build still does not complete.** It gets further, and then dies of
something else. Two fixed-arm runs, two different terminal failures, neither of them
this class and neither attributed:

- **run 1** — `rust-lld` produced a **zero-byte** `build_script_build` and cargo's
  `spawn` of it returned `ENOEXEC` ("Exec format error (os error 8)"). A 0-byte output
  from a linker that reported success is a write/flush problem, not a thread-lifetime
  one.
- **run 2** — reached 58+ crates (well into the `akuma-*` crates themselves) and then
  **cargo itself** took a `SIGSEGV in clone_thread`:

  ```
  [Fault] Process 778 (/usr/local/bin/cargo) SIGSEGV after 4.54s
  [Fault] SIGSEGV in clone_thread, calling exit_group
  ```

  That is the separately-tracked thread-spawn class —
  [`../runbooks/debug-thread-spawn-segv.md`](../runbooks/debug-thread-spawn-segv.md),
  which `debug-futex-lost-wakeup.md` §4 already names as its own open bug.

So the honest summary is: this fix removes one whole failure mode (processes stranded
with no thread, and the parent `wait4` hangs behind them) and the build now survives
long enough to hit the next one. **Do not read this doc as "`-j4` is green".**

## Background

- [`STALE_THREAD_SLOT_KILL.md`](STALE_THREAD_SLOT_KILL.md) — the 2026-08-02 original;
  §3.2's teardown inventory and §5.1's "do not reapply the consistency fix".
- [`TRAMPOLINE_STALE_PROCESS_RELR.md`](TRAMPOLINE_STALE_PROCESS_RELR.md) — the same
  class in the thread-entry trampoline, fixed the same day as this one.
- [`../runbooks/debug-futex-lost-wakeup.md`](../runbooks/debug-futex-lost-wakeup.md)
  — §4/§4a, whose "zero `[kill]` lines" check §4 above corrects.
