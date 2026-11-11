# B12: AArch64's slot table behind the interface the x86 kernels use (proposal, 2026-11-02)

> **Archived 2026-11-11 from `proposals/`.** Done by another route: the AArch64 kernel's scheduler
> runs behind the shared `ArchHooks` (registered from `crates/akuma-kernel-glue/src/hooked.rs`, "hooked"
> scheduler). The separate `akuma-sched-aarch64` crate this proposed (S4) was not created. The status
> line below is as last written.

**Status: proposal, not started.** Step B12 of [`docs/archive/TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md).
Written before any code moves because this step changes the AArch64 scheduler, the code behind most of the SMP
incidents in `docs/archive/` (the ON_CPU gate, the TTBR gate, the adopted idle-stack clobber, the phantom SVC).

## Where things are

`akuma-threading` (`src/lib.rs`, 8,039 lines) holds the thread table for every kernel, but the two architectures reach it
differently:

- **x86_64 is the guest.** Its machine effects are a registered struct, `X86ArchHooks` (`switch_to`, `current_slot`,
  `set_current_slot`, `idle_slot`, `can_run`, `transfer_lock_depth`, `allow_tick`, `write_user_context`,
  `read_user_context`, `prepare_task_slot`), built for both x86 kernels by `akuma_sched_amd64::hooks::<P>()` (A5). Its
  picker is `x86_pick_next`, its switch a cooperative stack switch. 47 `#[cfg(target_arch = "x86_64")]` blocks.
- **AArch64 is the default path**, not behind any interface: the `global_asm!` trampolines (`thread_start`,
  `thread_start_closure`), the fake IRQ-return frame a spawn builds, `sgi_scheduler_handler_with_sp` (EOI, the voluntary
  flag, `POOL` held across the whole save/load, the TTBR0 write and its L0 tripwires, the per-thread exception stack),
  the picker `ThreadPool::schedule_indices`, and 32 raw `get_context`/`get_context_mut` derefs. 18
  `#[cfg(target_arch = "aarch64")]` blocks mark only where it would not build for x86_64.

So one crate carries two schedulers' worth of mechanism, and only one of them is behind the decide/perform split the
rest of the tree uses.

## Goal

`akuma-threading` owns the table and the decisions (claim, states, park/wake, the picker); each architecture's crate
owns its effects behind one hooks struct. AArch64's go to a new `akuma-sched-aarch64`, beside `akuma-sched-amd64`. The
AArch64 switch *mechanism* does not change: SGI, fake IRQ frame, `POOL` across the switch, the same tripwires. The
change is where the code lives and what interface it is reached through.

## Steps, each gated on its own

| step | what | gate |
|---|---|---|
| S1 | **Characterize, move nothing.** Host tests that pin both pickers' decisions on the same table states (a differential like `akuma-net-yarn`'s), and a list of every way they differ | host tests |
| S2 | **One picker.** The differences become named policy fields (pinning, idle choice, the ON_CPU skip, fairness), as `akuma-net-yarn`'s `WaitPolicy` did; each divergence is kept and pinned, not unified, unless measured | host tests; AArch64 boot suite SMP=1 and SMP=4; amd64 self-tests x3 |
| S3 | **One hooks struct.** `X86ArchHooks` becomes `ArchHooks` with the fields both need (switch effects, current/idle slot, `can_run`, lock depth, user context, slot preparation, the tick window). AArch64 registers its answers from the existing code, still in `akuma-threading` | as S2 |
| S4 | **Move the AArch64 effects** to `akuma-sched-aarch64`: the trampolines, the frame builder, the TTBR0/exception-stack writes and their tripwires. `sgi_scheduler_handler_with_sp` becomes the picker plus `hooks.switch_to`. The `unsafe` moves with them (budget shifts, total should not grow) | as S2, plus the SMP stress below |
| S5 | **Delete the `cfg` blocks** that only separated the two | as S4 |

SMP stress for S4 and S5: `scripts/forktest_smp_matrix.py`, `scripts/test_sched_bklfree_ticket_fix.py`, the `no-bkl-*`
A/B toggles, and an AArch64 self-host kernel build at `-j4`, SMP=4 (`scripts/run_selfhost_kernelbuild.py`). Taoxichuan's
battery, tokioprobe at `smp=4` and a fixed point on the ryzen close the step.

## Risks

- The tripwires are the substance: the TTBR0 L0 checks, `l0_recently_freed`, the PICKED-TERMINATED print, the POOL
  try-lock skip. Each records a past incident; S4 moves them verbatim and S1's list says which picker decision each
  guards.
- `POOL` is held across the AArch64 save/load so that no peer can restore a stale SP. An interface that releases it
  between "decide" and "perform" reopens that window. The hooks run under `POOL`, as `switch_to` does on x86.
- The x86 picker skips a slot another core is still on (`can_run` plus the ON_CPU gate); AArch64's has its own form of
  that check. S2 must not lose either.

## Not in scope

Changing the AArch64 switch mechanism, the BKL protocol, or tick policy. The `BorrowCell` change for the raw
`get_context` derefs is the `unsafe` pass's, after B14.

## Open questions

1. Crate name: `akuma-sched-aarch64` matches `akuma-sched-amd64`. Alternative: fold the AArch64 effects into
   `akuma-exceptions`, which already owns the SGI vector, at the cost of a crate that is no longer only exceptions.
2. Is S2 (one picker) in scope, or only the effects split (S3-S5)? The effects split alone removes the `cfg` blocks but
   keeps two pickers in one crate.
