# `akuma-sched-amd64`: the machine half of the scheduler, extracted (2026-10-31)

**Stability: C** - slice 1 of a convergence; slice 2 (Taoxichuan) is not done.

`amd64/src/sched.rs`'s hook section is now a crate both amd64 kernels can run. `akuma-threading` stays the scheduler
(state machine, picker, park/wake); the crate is what a switch performs on the CPU: the per-slot `Machine` table
(page-table root, trap stack, user context, FP state, pin, affinity), `FxArea` and the `fpu_*_live` instructions, and the
`switch_to` / `can_run` / `prepare_task_slot` hook bodies, generic over a `Platform` each kernel implements.

## The seam
`Platform` carries what differs per kernel and nothing else: the user-context type (`UserCtx` on the metal kernel), the page-table root
(`kernel_root` / `active_root` / `activate`), the TSS `rsp0`, the per-CPU slot and `uctx` pointer, `FS`/`GS` bases out of the user context,
the freed-root ring, and a set of diagnostic callbacks that default to nothing. amd64's implementation (`struct Amd64` in `sched.rs`) wires
the diagnostics to the counters it already had, so the behaviour on the metal kernel is unchanged: same order of effects in `switch_to`
(root, `rsp0`, `uctx`, `FS`/`GS`, lock/canary checks, FP save/restore), same `can_run` rule, same `prepare_task_slot` field resets.

## What moved, what stayed
Moved: `FxArea` and the FP instructions, `Machine`/`Stacks`, `RESERVED_CORES`, the three hook bodies, the `XSAVE_ON` / `XCR0_ENABLED` statics
(`smp.rs` re-exports them). Stayed in `sched.rs`: preemption, park/wake wrappers, the idle loop, spawn/finish, the user-context syscalls
glue, every self-test and counter. Public names are unchanged (`sched::FxArea`, `sched::fpu_save_live`, ...), so no caller moved.

## Verification
`cargo build -p akuma-amd64 --features no-tests` (the metal configuration). Booted on the ryzen under Linux KVM (QEMU microvm, the Akuma root
partition as the disk) at 1 and 4 CPUs: reaches `sshd`, five fork/pipe/`wait` probes pass over ssh at 4 CPUs, zero `SWITCH`, `FREED-CR3`,
`NO-BKL`, `PANIC` lines. **Not yet run on metal.** The default (self-test) configuration of `akuma-amd64` does not build on the tree as it stood - five
errors in `mem.rs`, `paging.rs` and `kvm/api/vcpu.rs`, none in this change - which is why the metal kernel is built `--features no-tests`.

## Open
Slice 2: Taoxichuan's `exec_twin` hooks to this crate. That needs its per-thread user context to be a `Platform::User`, which is the end of the
nested-crossing model (`switch-to-crates.md` S3). Until then Taoxichuan keeps its own hooks.
