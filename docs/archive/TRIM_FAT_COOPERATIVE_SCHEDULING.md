# Trim fat: cooperative scheduling removal

**Date:** 2026-08-11. **Scope:** implements the removal audited in
[`COOPERATIVE_SCHEDULING_AUDIT.md`](COOPERATIVE_SCHEDULING_AUDIT.md) — read that
doc first for the "why" and the file:line inventory this doc executes against.
**Status: DONE.** Both Scope A (dead API surface) and Scope B (full flag
removal, thread 0 made preemptible) landed in one pass. Boot-verified on
devbox-smoltcp (sshd + network) and the default self-test profile; found and
fixed two latent test bugs the removal exposed (§4).

**One line:** the kernel scheduler has no cooperative class left — thread 0 is
now an ordinary preemptible thread like every other slot, `ThreadSlot`'s
`cooperative`/`timeout_us` fields are gone, and so is the whole
`spawn_fn_cooperative` API surface. Only the userspace rump fiber scheduler
(a separate, unrelated cooperative scheduler — audit §5.3) still exists in the
codebase.

## 1. What was removed

Both scopes from the audit's §7 tables, in one commit rather than staged:

### Scope A — dead API surface

- `spawn_fn_cooperative`, `spawn_fn_with_options`,
  `spawn_user_thread_fn_with_options` — all deleted. `spawn_fn` now calls
  `spawn_user_thread_fn_internal(f, false)` directly (`threading/mod.rs:2811`).
- `make_idle_preemptible` — deleted (zero callers, confirmed by grep before
  removal, matching the audit's finding).
- `test_cooperative_timeout`, `test_spawn_cooperative`,
  `test_mixed_cooperative_preemptible` — deleted from `src/tests.rs`, along
  with `COOP_THREAD_DONE`/`PREEMPT_THREAD_DONE` and their accessor helpers
  (only consumers were the deleted tests).
- `test_thread_last_core_tracked` (`src/process_tests.rs:813`) — edited, not
  removed, per the audit's note: `spawn_fn_cooperative` → `spawn_fn`.

### Scope B — the flag itself

- `ThreadSlot.cooperative: bool` + `.timeout_us: u64` — removed
  (`crates/akuma-exec/src/threading/types.rs`). `start_time_us` stays; it has
  other readers (`commit_switch`, the network-boost path, `idle_halt`'s
  halted-quantum correction) unrelated to the cooperative class.
- `COOPERATIVE_TIMEOUT_US` — removed (`types.rs`).
- `KernelThreadInfo.cooperative` + `ThreadPoolSnapshot.cooperative` — removed;
  `list_kernel_threads`'s thread-naming heuristic no longer has a
  `"cooperative"` name (that arm was already dead in production — the audit
  confirmed only thread 0 was ever cooperative, and thread 0 is caught by an
  earlier `i == 0` match arm).
- `dump_stack_info`'s print format dropped the `Cooperative: {}` field.
- The `schedule_indices` cooperative-skip block (audit §2,
  `threading/mod.rs:2481-2494` pre-removal) — deleted, along with the
  `current_cooperative`/`current_timeout_us`/`current_start_time_us` locals it
  was the only reader of, and the now-unused `current_state` read.
- `ThreadPool::init`'s thread-0 setup (`threading/mod.rs:2211`) no longer sets
  `cooperative = true` / `timeout_us = COOPERATIVE_TIMEOUT_US`; the doc comment
  above it now says "preemptible" instead of "cooperative for I/O protection".
- `adopt_current_as_core_idle` and `cleanup_terminated_internal` no longer
  clear a `cooperative`/`timeout_us` that doesn't exist.
- `spawn_user_thread_initializing` / `spawn_user_closure_initializing` /
  `spawn_user_thread_fn_internal` / `spawn_system_thread_fn` all dropped their
  `cooperative: bool` parameter. All call sites (`process/mod.rs` fork/vfork/
  clone_thread, `src/process_tests.rs`) already passed `false`, so this is a
  pure signature simplification, not a behavior change at any of them.
- Three doc comments that explained *why* a voluntary SGI needed to "bypass
  the cooperative-idle guard" (`threading/mod.rs`, near `VOLUNTARY_SCHEDULE`,
  `request_voluntary_reschedule`, and `schedule_blocking`) were rewritten —
  the mechanism they described (bypassing the now-deleted cooperative-skip
  check) is gone; what's left is the preemption-disabled bypass, which was
  always the other half of what `voluntary=true` short-circuits.
- `crates/akuma-exec/src/sync.rs`'s `lock_bounded` doc comment no longer
  claims a "`cooperative` thread... is immune to *involuntary* preemption by
  design" — that claim is simply false now, and the surrounding argument
  (voluntary yield beats waiting on re-enabled preemption alone) doesn't
  depend on it.

The async executor `memory_monitor`/`Timer`/`schedule_wake` machinery the
audit's §7 called "a separate ~170 LOC win, not counted in this table" was
**not** touched — it's a distinct removal candidate (the last kernel `async`),
out of scope here.

### Diff stats

```
crates/akuma-exec/src/sync.rs            |   9 +-
crates/akuma-exec/src/threading/mod.rs   | 125 ++----------
crates/akuma-exec/src/threading/types.rs |   9 -
src/config.rs                            |   2 +-
src/process_tests.rs                     |  41 ++--
src/tests.rs                             | 320 ++++++++-----------------------
6 files changed, 136 insertions(+), 370 deletions(-)
```

`src/tests.rs`'s churn is dominated by the three deleted tests (~180 LOC) and
the ThreadWaker test rewrite (§4) — deletions there aren't 1:1 with the
audit's Scope A/B LOC tables since fixing the exposed bugs added code back.

## 2. Verification

- `cargo build --release`, `cargo build` (dev), `cargo check`, `cargo clippy
  --release` — all clean, zero warnings.
- `scripts/build_extreme_size.sh` — extreme-size profile (single-core, 4 MB
  floor) still builds: 569 KB image, well under the floor.
- Host unit tests (`cargo test --target <host>`, plus `akuma-ssh-crypto` under
  `userspace/`) — all passing, unaffected (this code isn't host-testable
  directly; the check here is that nothing else broke).
- **Full in-kernel boot self-test suite** (`INSTANCE=1 cargo run --release`,
  default single-core profile that runs `src/tests.rs`/`src/process_tests.rs`
  at boot) — all sections pass after the two fixes in §4, including
  `Threading Tests: ALL PASSED` and every `process_tests.rs` section, through
  to normal herd/sshd/httpd startup with no `FAILED`/`PANIC`/`HALTING`
  anywhere in the log.
- **devbox-smoltcp boot** (`scripts/build_devbox_smoltcp.sh` +
  `overlays/devbox/run-smoltcp.sh`, `SMP=2`, `MEMORY=4096`) — boots clean,
  `[herd] Started sshd` observed, SSH login works.
- **Network sanity check**: from inside the devbox-smoltcp guest, downloaded
  `bootstrap/music/soundtrack/tokyo_rider_omegashima.flac` (411,840,356 bytes)
  over HTTP from a server on the host (reached via the QEMU user-net gateway,
  `10.0.2.2`). Transfer completed in ~38 s (~11 MB/s) and the guest-side MD5
  (`2f8ef5d8e3a8c1e5387741be1262cc84`) matched the host file exactly — this is
  the empirical check the audit's §6 called for ("measure sshd echo latency
  with slot 0 set `cooperative = false`"), done as a sustained transfer rather
  than an echo-latency probe, which exercises the now-preemptible thread 0's
  network-poll path under sustained load rather than just a handshake.

No regression found in the removal itself. Two latent bugs it *exposed* are
covered next.

## 3. Why "measure sshd echo latency" wasn't the risky part

The audit's §6 framed the remaining risk as empirical: does userspace sshd's
poll-driven network path tolerate losing thread 0's 100 ms grace window? The
`NETWORK_THREAD_RATIO=4` proportional boost (independent of the cooperative
flag, keyed off `NETWORK_THREAD_ID`) already schedules the network-poll thread
every 4th tick regardless, so this was expected to be a non-issue — and it
was: the 412 MB transfer above went through at full expected throughput with
no observable degradation.

The actual risk that materialized was different in kind: **kernel self-test
code that fabricated scheduler state directly**, relying on thread 0's old
immunity to involuntary preemption as an implicit correctness crutch. That
crutch had nothing to do with sshd or the network path — it was purely about
what the *boot self-test suite itself* could get away with while running on
thread 0.

## 4. Two latent bugs the removal exposed

### 4.1 `[SGI-S FATAL] new_sp=0x0` — fabricated thread state on a bare slot

`test_thread_waker_marks_ready`, `test_thread_waker_idempotent`, and
`test_thread_waker_roundtrip` (`src/tests.rs`, ThreadWaker tests) each picked
an unused (`FREE`) thread slot and poked its atomics directly — `WAITING`,
then fire a `ThreadWaker`, expect `READY` — without ever spawning a real
thread into that slot. A `FREE` slot's `Context.sp` is still `0`: nothing had
ever called `setup_fake_irq_frame` for it.

On the cooperative scheduler this was reckless but silent: thread 0 (running
the test suite) was immune to involuntary preemption for up to 100 ms, so the
window between "mark the phantom slot READY" and "restore it to FREE" almost
never coincided with an actual scheduler dispatch. With thread 0 fully
preemptible, a timer tick landing in that window now round-robins straight
into the phantom `READY` slot — and the context switch aborts with
`[SGI-S FATAL] new_sp=0x0 invalid!` (`threading/mod.rs:3054`), hanging the VM
at 100% CPU. Reproduced deterministically on the first post-removal self-test
boot (single-core, default profile).

This is not a new failure mode — `src/process_tests.rs`'s
`test_kill_thread_group_reaps_futex_blocked_sibling` already carries a doc
comment describing the exact same crash ("a bare claimed slot fabricated into
WAITING gets dispatched with context sp=0... seen 4/4 suite runs at SMP=1..4
on 2026-07-23") and had already been written to avoid it, by spawning a real
thread via `spawn_user_thread_initializing` instead of poking a `FREE` slot.
The three ThreadWaker tests just hadn't been written against that lesson.

**Fix:** all three tests now spawn a real slot via
`spawn_user_thread_initializing` (kept `INITIALIZING`, never marked `READY`
by the spawn itself) with a defensive trampoline
(`waker_test_park_trampoline`) that self-terminates if it's ever actually
dispatched. This gives the slot a real, valid `Context` (`sp`/`ttbr0`/`elr`
all set by `setup_fake_irq_frame`) before the test fabricates `WAITING`/
`READY` state on it, so a stray dispatch lands somewhere safe instead of
crashing. Reclaim uses `mark_thread_terminated` + `cleanup_terminated_force`
(matching the futex-sibling test's own cleanup comment: "a hard external
terminate is safe" for a slot that was never really dispatched).

### 4.2 Flaky READY→TERMINATED race, once the crash was fixed

Fixing 4.1 traded a crash for a *correctness* race: the spawned slot is now
genuinely schedulable once marked `READY`/woken, and its defensive trampoline
self-terminates the instant it's dispatched. An involuntary preemption
between "fire the waker" and "read the resulting state" could now let the
slot actually run and self-terminate before the test observes it — flipping
the read from the expected `READY` (1) to `TERMINATED` (3). Confirmed on the
second post-fix boot: `test_thread_waker_marks_ready` and
`test_thread_waker_roundtrip` both failed with `state=3 (expected 1)`,
`test_thread_waker_idempotent` passed only because that run happened not to
hit the window.

**Fix:** wrap "fabricate state → fire waker → read state" in
`disable_preemption()`/`enable_preemption()` in all three tests. On the
single-core profile the self-test suite normally boots under, this fully
closes the window (an involuntary tick on the only core is gated by the
*current* thread's own preemption-disabled counter, which is what's held
here). Real multi-core SMP could in principle still race via a peer core's
independent scheduling decision — the same caveat that already applies to
`test_schedule_blocking_respects_terminated`'s own note ("we test this at the
atomic level... since the invariant is purely about the atomic state
machine"); not hardened further here, consistent with how the rest of the
suite treats this class of test.

### 4.3 `test_thread_slot_reclaim_on_spawn`: a timing assumption, not a crash

`src/process_tests.rs`'s `test_thread_slot_reclaim_on_spawn` filled the thread
pool, let every thread self-terminate, then called `reclaim_terminated_slots()`
immediately and asserted **zero** slots were reclaimed — reasoning that
`THREAD_CLEANUP_COOLDOWN_US` (10 ms) was "far longer than this call takes".
That assumption depended on thread 0 running the fill-and-yield loop with no
involuntary detours of its own. Once thread 0 could itself take a timer-tick
detour mid-loop, the loop's wall-clock time was no longer reliably under
10 ms, and the assertion started failing with `hot_reclaim=20` (i.e. the
kernel correctly reclaimed 20 slots that had, in fact, been outside their
cooldown for a while — not a bug in `reclaim_terminated_slots` itself).

**Fix:** record `uptime_us()` at the start of the fill-and-yield loop (the
earliest possible termination timestamp for any spawned thread) and only
assert zero-reclaim when the measured gap to the reclaim call is provably
under the cooldown. When it isn't, a nonzero reclaim is accepted as correct —
the cooldown itself is still enforced unconditionally inside
`reclaim_terminated_slots`, independent of this test's timing luck. Verified:
a later boot reclaimed 43 slots hot and still reported `PASSED`.

## 5. Compiled / runtime impact

Not separately measured against the audit's ~600 B BSS / ~30 B text estimate
(§7) — the point of this pass was correctness and source clarity, not size,
and the audit already called the size delta "not measurable on a single
build; the floor fluctuates more than this between toolchain versions."
`scripts/build_extreme_size.sh`'s 569 KB result is consistent with "no
meaningful change."

## Background

- [`COOPERATIVE_SCHEDULING_AUDIT.md`](COOPERATIVE_SCHEDULING_AUDIT.md) — the
  survey this doc implements; file:line inventory, the mechanism's history,
  and the removal-cost tables (§7) this doc's §1 executes against.
- [`docs/reference/subsystems/scheduler.md`](../reference/subsystems/scheduler.md)
  — current-state scheduler doc; already described the scheduler as uniformly
  preemptive and needed no changes from this removal (audited, see commit
  history for this doc).
- `src/process_tests.rs`'s `test_kill_thread_group_reaps_futex_blocked_sibling`
  doc comment — independently documents the exact `[SGI-S FATAL] new_sp=0`
  failure mode this pass hit in the ThreadWaker tests (§4.1), predating this
  removal by about a month.
