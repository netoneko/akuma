# AArch64 fork/clone skipped the slot reclaim on exhaustion (2026-11-11)

## Symptom

Under QEMU HVF on the Mac at `SMP=1`, the AArch64 boot suite failed one test on every
run. The test images were M7b (`e56d2c7a`) and the test move (`6c06c534`), 7 of 7 runs
between them:

```
[Test] thread_slot_reclaim_on_spawn_initializing FAILED: spawn refused with
"No free x86 task slots" instead of reclaiming (filled 254 slots)
```

On the Lima KVM rig the same test reported PASSED or INCONCLUSIVE. There the idle
collector usually frees the slots before the spawn needs them, so the spawn never meets
an exhausted pool. A historical tally of the scratch logs from this branch: 68 PASSED,
74 INCONCLUSIVE, 5 FAILED. All 5 failures were HVF runs.

## Cause

`spawn_user_thread_initializing` is the spawn entry point that `fork`, `vfork` and
`clone_thread` go through. On a miss it runs `reclaim_terminated_slots()` and retries
once, but only when the error is `"No free user thread slots"`
(`docs/reference/subsystems/thread-lifecycle.md` §2.1).

Since the hooked scheduler became the only AArch64 scheduler (`cd1d8f17`), the attempt
it retries is the shared `ThreadPool::spawn_user_closure_initializing`. That function
was written for amd64 and answered exhaustion with `"No free x86 task slots"`. The
distinct string was deliberate, and its doc stated the reason: `claim_slot` "already
takes a `TERMINATED` slot no core is executing", so a retry could only fail again.

That reasoning holds only without `akuma-threading`'s `collector-recycles` feature.
The AArch64 kernel enables it (`crates/akuma-kernel-glue/Cargo.toml`), and with it
`claim_slot` takes only `FREE` slots. So on AArch64 a pool full of cooled-down
`TERMINATED` slots, exactly the state the retry exists for, reached userspace as
`EAGAIN` from `fork`/`clone`. This is the 2026-08-03 failure mode of
`SELFHOST_DEVBOX_SMOLTCP.md` (a `pthread_create` loop dying at iteration ~58)
reintroduced by a string mismatch.

## Fix

`spawn_user_closure_initializing` returns `NO_FREE_USER_SLOTS`, the constant the retry
now matches on, when `collector-recycles` is on. It still returns the amd64 string
when the feature is off. The amd64 behaviour is unchanged, since `cfg!` folds to the
old string there.

## Verification

QEMU HVF on the Mac, `SMP=1`, the boot suite, the same rig at the same time:

| image | `..._initializing` |
|---|---|
| `6c06c534` (pre-fix) | FAILED 4/4 |
| fixed | PASSED 4/4: `slots exhausted (live=0 terminated=248 ceiling=248) — reclaimed 254 and retrying` |

At `SMP=2` and `SMP=4` on HVF the fixed image gives 323/0 and 321/0 (two runs each), as
before.

## Still open

- `thread_slot_reclaim_on_spawn` (the `_fn_internal` sibling) failed on **both** images
  in the four-run batch above, 8 of 8, with `respawn_ok=false` and the helper thread
  never having run. Twenty minutes earlier it had passed on the same pre-fix image,
  4 of 4. So it is host-state dependent, not this change. Its fill threads did not run
  during 200 ms of `yield_now` from thread 0 at `SMP=1`. Not yet investigated.
  **This is not the failure the earlier docs explain.** Every recorded failure of this test
  before this one was `hot_reclaim=N (want 0)`, the fill loop outliving the 10 ms
  cooldown. That was a flaw in how the test was set up, fixed by moving the cooldown
  baseline (`TRIM_FAT_COOPERATIVE_SCHEDULING.md` §4.3, `HOOKED_EL1_PREEMPTION.md`; also
  `PAGE_TABLE_UAF_BKL_STORM.md`, `J4_HANG_LIVE_AUTOPSY.md`, `AARCH64_SMP_HOOKS_RACE.md`).
  This failure is `hot_reclaim=0` with `respawn_ok=false` and
  `gated_from_other_thread=usize::MAX`: the hot-reclaim assertion held, and no spawned
  thread ran at all.

## The `thread_slot_reclaim_on_spawn` failure: the second entry point (resolved 2026-11-11, later the same day)

The open item above was the same defect, reached through the other user spawn entry point.

`spawn_user_thread_fn_internal` serves `spawn_user_thread_fn` and
`spawn_user_thread_fn_for_process`, so it creates every `spawn_process` thread. It had taken
amd64's shape: claim through `spawn_user_closure_initializing` once, then `return` its result.
The claim/reclaim/retry body it used to run sat below that `return` as unreachable code, behind
`#[allow(unreachable_code, unused_variables)]`. So no reclaim ran on this path at all.

With `collector-recycles` on, that explains the whole failure shape:
- The fill loop stops at a pool of `TERMINATED` slots.
- The gate helper's spawn is refused, so `gated_from_other_thread` stays `usize::MAX`.
- After the 200 ms cooldown, the respawn is refused too: `respawn_ok=false`.

On the Lima rig, the idle collector usually frees slots first, which is why it passed there and
looked host-dependent. In production, process creation (`spawn_process`, herd's services, boxed
processes) could fail while slots were only waiting to be collected.

**Fix:** the retry moved out of `spawn_user_thread_initializing` into
`claim_user_slot_with_reclaim`, which both entry points call. The unreachable body is deleted.

**Still open:** `start_irqs_disabled`, the `_for_process` variant's guard against a tick before
the user TTBR0 is set, is ignored on this path on both kernels. That was already so before this
change; it was the case the unreachable AArch64 body handled.

## Background

[`../reference/subsystems/thread-lifecycle.md`](../reference/subsystems/thread-lifecycle.md) §2.1 ·
[`SELFHOST_DEVBOX_SMOLTCP.md`](SELFHOST_DEVBOX_SMOLTCP.md) ·
[`AKUMA_MULTIARCH_ATTEMPT_0.md`](AKUMA_MULTIARCH_ATTEMPT_0.md)
