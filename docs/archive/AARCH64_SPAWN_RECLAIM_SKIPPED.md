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

## Background

[`../reference/subsystems/thread-lifecycle.md`](../reference/subsystems/thread-lifecycle.md) §2.1 ·
[`SELFHOST_DEVBOX_SMOLTCP.md`](SELFHOST_DEVBOX_SMOLTCP.md) ·
[`AKUMA_MULTIARCH_ATTEMPT_0.md`](AKUMA_MULTIARCH_ATTEMPT_0.md)
