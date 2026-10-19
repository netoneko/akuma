# AArch64 `SMP>=2` self-test image: "ExceptionHooks not registered" (intermittent)

**Status: FIXED 2026-10-19.** Latent since `f9808bd6` (2026-09-01, "extract akuma-exceptions"), which
made the IRQ path call `hooks()` on a `Registered<ExceptionHooks>` cell.

## Symptom

`cargo build --release` (self-test image) at `SMP=2`/`4` panics early, sometimes:

```
[SMP-shared] CPU_ON core 1 (mpidr=0x1) -> ok
[SMP-shared] core 1 online (idle tid 1)
!!! PANIC !!!  Location: crates/akuma-not-even-once/src/lib.rs:208
Message: exceptions: ExceptionHooks not registered — call exceptions::register_hooks() before exceptions::init()
```

(the console interleaves the BSP's `[RNG]`/`Timer initialized` lines into the panic text.)
`SMP=1` and the runtime image (`no-tests`, `extreme-size`) never show it.

## Cause

`kernel_main` brings the secondaries up **before** `exceptions::register_hooks` in the self-test
image (deliberate: the `test_smp_shared_*` suites need them online;
`cfg(all(kernel_smp_shared, not(feature = "no-tests")))`). `secondary_shared_start` then installs
VBAR, arms its timer and unmasks IRQs on its own, so its first tick runs `dispatch_irq` ->
`hooks()` -> `Registered::require()` on an empty cell. The comment above the registration ("no
exception can be handled before `exceptions::init()`") was true for the BSP only. Whether the BSP
registered inside one tick (~ms) of core 1 coming online depended on how slow the console was —
cold Lima/KVM lost, warm boots won. A second, quieter hazard on the same window: the secondary
armed its tick from `current_tick_us()` before the BSP's `timer::init` had published it.

## Measured (Lima KVM, 6 busy loops on the host to widen the window, 2 GB, 8 boots each)

| kernel | SMP=2 panics |
|---|---|
| `644f20d6` (HEAD before the tty commit) | **4 / 8** |
| `cbb6ca54` (2026-10-19) | 1 in the first 3 boots (stopped at first hit) |
| `1b17a8f5` (2026-10-18) | 1 in the first 4 |
| `6588a17c` (merge-base with `origin/main`) | 1 in the first 1 |
| **fixed** | **0 / 16** (8 x SMP=2, 8 x SMP=4) |

So it is **not specific to `more-amd64-fixes`**: main has it. Load only makes it likelier. (An
early "11/12 ok" run was invalid — `target/aarch64-unknown-none/release/akuma` had been overwritten
by the pre-commit hook's extreme-size build, i.e. the runtime image, where the race cannot occur.)

## Fix

`smp_shared::BSP_IRQ_READY` + `open_secondary_irq_gate()`: `secondary_shared_start` spins (no lock,
no BKL, no IRQ needed) after adopting its idle slot and **before** `gic::secondary_init`/VBAR/timer/
unmask; `kernel_main` opens the gate after `exceptions::register_hooks`, `exceptions::init` and
`timer::init`. In the runtime image secondaries start later and the wait is one load. Doing this
rather than registering the hooks earlier keeps the pre-existing guarantee that a secondary only
takes ticks once the BSP can handle them.

## Verified

Fixed kernel, full suite at `SMP=2`: 309 `PASSED`; failures `test_epoll_socket_waker` (known,
latency) and `thread_slot_reclaim_on_spawn: hot_reclaim=98 (want 0)`. The unfixed kernel on a boot
that got past the race: 312 `PASSED`, same two plus `orphan reparented to init` (a timing flake).
`thread_slot_reclaim_on_spawn` is therefore a **pre-existing `SMP>1` failure**, never seen before
because `SMP=2` would not reliably boot — open, not investigated.

## Similar earlier problems

None recorded for this cell. The class — a `Registered<T>` read before its `register_*` — is the
"boot-order bug" `docs/reference/subsystems/kernel-hooks.md` says `require()` exists to surface;
earlier incidents in the archive (`ExecRuntime`/`ExecConfig`/`NetRuntime` "not registered") were
single-core ordering mistakes found by always-reproducible panics. This is the first one that
depends on a second core's timing.
