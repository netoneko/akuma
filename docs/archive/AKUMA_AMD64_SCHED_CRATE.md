# `akuma-sched-amd64`: the machine half of the scheduler, extracted (2026-10-31)

**Stability: C** - slice 1 (amd64) and slice 2 (Taoxichuan's switch, FP state and `can_run`) are done; the process model and init are not.

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

## Slice 2: Taoxichuan (2026-10-31)
`taoxichuan/src/sched.rs` is `struct Tx: Platform` over `User = ()` (its syscall stub saves registers on the slot's own band, so there is no per-thread record to
publish). `exec_twin`'s hand-written `switch_to`, its legacy-`fxsave` area and its `can_run` are gone: the crate switches the root, `rsp0`, `FS`/`GS` and FP state, and
decides `can_run`. What stayed Taoxichuan's is passed through overridable `Platform` hooks added for it, with defaults that leave amd64 unchanged:
`space_root_of` (the root comes from the task record), `trap_top_of` (per-slot trap stacks in `main.rs`), `after_install` (the syscall band and the dispatcher's task record),
and `RELOAD_SAME_ROOT` - the crate skips the `cr3` write when the root is unchanged because the metal kernel's freed-root ring proves the frame was not recycled;
Taoxichuan has no such ring, so it keeps the write, which doubles as the flush a reissued root frame needs. Every idle machine is marked `idle` and pinned to its core,
so the crate never offers it to a picker. Verified on the ryzen KVM rig: `smp=1` 8/8, `smp=2` 8/8, `smp=4` 13/14 over four boots (the miss: a session accepted and reaped before it ran anything), zero faults.

Found on the way: the trace build's idle-lap hang dump ran on every idle core with no lock and panicked with `BorrowCell mutably borrowed` when four cores reached the lap count together; it is now cpu 0 only, under the lock.

## Open
`prepare_task_slot` and the user-context record are still Taoxichuan's own (`child.rs`); adopting the crate's needs the slot's `CrossSave` and bands to become the per-thread record, which is the end of the
nested-crossing model (`switch-to-crates.md` S3). `init` (`sched::init`: record the kernel root, pin the boot thread, adopt it) is not extracted yet on either kernel.
