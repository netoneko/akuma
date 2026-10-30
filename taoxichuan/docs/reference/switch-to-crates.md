# Switch to the standard process slots and scheduler — the target architecture

**Stability: C** (a decision doc: the target is decided, the slices are not yet
executed). Plan: `PLAN-pm.md` at the repo root (session pm). Everything here was
checked against the tree and `amd64/src/` on 2026-10-29 unless marked UNCHECKED.

## The decision

Taoxichuan stops hand-rolling process slots, the spawn crossing and its scheduler
scaffolding. Every process/scheduling decision moves to the crates the other
kernels use — `akuma-exec` (+ `akuma-exec-core`, `akuma-slot-table`,
`akuma-threading`), `akuma-fdtable`, `akuma-pipes`, `akuma-syscalls-abi` /
`akuma-syscalls-glue` — and `taoxichuan/src/` keeps only target glue (trap entry,
CR3/fs effects, device arms), the way `amd64/src/` is composed today.

What dies: the `SpawnSlot` ring and `CrossSave`/`SlotCross` records
(`src/child.rs:40-265`), the M2 SPAWN crossing (`child.rs:919-1454`:
`spawn_arm`, `exec_replace`, `child_enter`, `child_exit`, `exit_cross`), the
private `SPAWN`/`WAITPID`/`CLOSE_CHILD_STDIN` syscalls, `PARENT_OF`
(`child.rs:643-664`), the private wait scans (`child.rs:1652-1855`), and
`exec_twin.rs`'s coupling of the hooks to the ring. ≈1,600-1,900 lines deleted in
total; the twin's hook tables, `switch_to`'s five machine effects and the fs
shadow stay (they are the kernel glue the crates require — amd64 keeps the same
kind of code in `amd64/src/sched.rs` and `exec_runtime.rs`).

## The target, in five rules

1. **init is the first `Process`, with a real slot** — registered at boot like
   amd64's `run_init` (`amd64/src/usermode.rs:8845`: `register_exec_process(1, …)`,
   `reserve_pid_floor(2)` first, `exec_runtime.rs:625-632`). `fork`, `execve`,
   `wait4` then work from init; today they answer `EPERM`/`ENOSYS` from the boot
   task (`child.rs:868-886`), which is the only reason sshd needs private `SPAWN`.
2. **sshd spawns the Linux way** — built with `libakuma`'s `linux-abi` feature:
   `pipe2` + `fork` + `dup3` + `execve` (`userspace/libakuma/src/lib.rs:2196-2230`),
   collected with `wait4(WNOHANG)` (`:2493-2513`). No private syscall. `clone`
   keeps working for musl `posix_spawn` by routing through
   `akuma_exec::process::fork_process` with the child-stack argument (amd64's
   shape, `usermode.rs:5585`).
3. **Syscall numbers come from `akuma-syscalls-abi`** — the legacy x86_64 numbers
   (mkdir 83, rename 82, unlink 87, rmdir 84, readlink 89, chmod 90, chown 92/94,
   symlink 88, …) are served by amd64's two-table shape: a legacy table of
   x86-only numbers translated to `Syscall::*` and routed into
   `akuma-syscalls-glue` (`usermode.rs:1511`, `:7506-7530` asserts the tables
   stay disjoint). No new private arms.
4. **The dispatcher keeps only marshaling** — the ~90-row table
   (`syscalls.md`) loses exactly four private rows (SPAWN, WAITPID,
   CLOSE_CHILD_STDIN, the M2 clone arm); ~30 fs/stat/dirent/time/uts/poll rows
   become `akuma-syscalls-*` family calls as stages need them; fd/net/fs rows
   already call crates and stay.
5. **The pool owns runnability** — init, children, sshd and idle are
   `akuma-threading` slots (`INITIALIZING → READY → RUNNING → WAITING`); the tick
   preempts ring 3 (live since M4 slice 3); waits park as `WAITING` slots woken by
   the crate's child-exit wake (amd64's `sys_waitpid` shape, `usermode.rs:6153+`);
   the idle thread halts and drains retired processes (amd64
   `sched.rs:1387-1426`). No suspended in-syscall frame exists anywhere — the
   `CrossSave`/`EXEC_SAVE` crossing and its lost-resume stall class are gone by
   construction.

## The slices (each one executor session, gates green at every step)

| slice | what | deletes | gate | rollback const |
|---|---|---|---|---|
| S0 | baseline battery on the merge tree | — | full battery green | — |
| S1 | init registered as pid 1 with a real slot (twin of `run_init`) | 0 | battery + `[init] registered pid=1` | `INIT_AS_PROCESS` |
| S2 | wait4 → crate-owned park/wake over the process table; `PARENT_OF` deleted | ~165 L | PK1, P1-P5, F1, grandfork step 5 = ECHILD | `WAIT_VIA_CRATE` |
| S3 | sshd → `linux-abi`; the M2 crossing and the private syscalls die; clone → `fork_process` | ~700 L | ssh `sh -c`, apk add curl, curl HTTPS — first crossing-free apk boot | `SSHD_LINUX_ABI` |
| S4 | boot programs become fork+exec children of init; boot-task arms die; `PREEMPT_ARMED` re-derived | ~150 L | stage 1-3 lines + battery | `INIT_FORKS_PROGS` |
| S5 | `load_task` re-based on `akuma-elf-exec`'s decisions (PT_INTERP; stage-8 enabler) | ~200 L net | hello, F1, apk, curl | `LOAD_VIA_CRATE` |
| S6 | dispatcher two-table shape via `akuma-syscalls-abi` + glue (stage-9 enabler) | duplicated arms | battery + disjointness self-check | `GLUE_DISPATCH` |
| S7 | sweep: docs, diagrams, dead ring types | remainder | grep clean | — |

Cost: ~320 executor tool calls, ~10-13 boots (per-slice costs in `PLAN-pm.md` § 6).
Cut to half: S0, S5, S6, folding S7 into S3/S4 — the thesis set is S0-S3.

Known risks with their resolutions: `PLAN-pm.md` § 5 (stale slots/trap frames,
switch-while-masked, CR3 on switch, fd refcounts on fork, fs base on exec, FPU
state, orphan reaping, INITIALIZING-slot discipline, linux-abi being a
first consumer). The lost-wakeup stall (RESULT of session pl) is removed by
construction at S3 and only tolerated (retried, never scored FAIL) before it.

## Background

`PLAN-pm.md` (this switch's full plan), `../docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md`
(the process model), `../docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md` (the twin's
hook tables), `docs/reference/subsystems/scheduler.md` (the pool's slot state
machine), `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md` (the experiment).
