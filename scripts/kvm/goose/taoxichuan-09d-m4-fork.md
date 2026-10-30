# Taoxichuan stage 8, M4 session pd: slice 3 - real `fork` + `wait4` with a real task switch (gate: `yes | head | wc` runs concurrently)

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: `RESULT-pc.md`, origin `aa1b4cbe` (slices 1-2: the runtime twin is registered, the crossing statics are per-slot).
`git log --oneline -3` first: your branch should end at `b1acc953`. **Keep your context small.** Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`.
Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Build: `kbuild -p taoxichuan -j 4 --features no-tests`. Box log times are UTC.

## The goal
Today `fork` is M2's blocking scaffold: the child runs to completion inside the `fork` syscall, so a pipeline whose writer outlives one read (`yes | head`), or `git clone`'s helper pump, can never work. The amd64 kernel already runs the real thing through the shared crates
(`akuma-exec`'s `fork_process`, child exit channels, `wait4`, `akuma-threading`'s scheduler and hooks). **Make taoxichuan do the same: a forked child is a real task, `wait4` parks the parent until the child exits, a pipe reader/writer parks when it must and is woken by the other end, and the scheduler switches between tasks at those park points.**
You find and write the code; the docs below say what is decided and why. Keep M2's scaffold compiled and selectable (a `const` or feature) until the new path passes every gate; flip the default only when green and leave the old path for the next session to delete.
Decisions already made (do not reopen): preemption by timer is the NEXT slice (this session switches where a task parks, never in ring 0); the design is SMP-ready (no new `static` current task: go through the slot accessors in `taoxichuan/src/exec_twin.rs`); pull crates, do not copy code.

## Docs to read first (in order; `sed -n` the sections, never whole files)
1. `docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md` - your spec: §1 hook tables, §2 where the switch happens, §3 stacks vs crossing statics, §5 slice 3.
2. `docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md` §§ 2, 4, 6 (what amd64 consumes, the unknowns U1/U4/U5).
3. `RESULT-pc.md` and the agent log section "Stage 8, session pc" (the hooks table real/no-op/stub; why `exit_cross` is a naked function: the compiler must not touch registers around a crossing).
4. The amd64 fork story, for the bugs it already paid for: `grep -il "fork_process" docs/archive/*.md`, then `AKUMA_AMD64_COW.md`, `AKUMA_AMD64_FORK_VS_THREADS_SEGV.md`, `AKUMA_AMD64_5B_SLICE2.md`, `AKUMA_AMD64_C2_SLICES_6_7.md`.
5. `docs/reference/subsystems/` entries for the scheduler/smp and `docs/README.md`'s symptom matrix. When a gate fails, `grep -n -i "<error text>" docs/README.md docs/runbooks/*.md` BEFORE reading source, and write the matching rows (or "no row") in RESULT-pd.md.
**Probes already exist - use one before writing a new one**: `userspace/forkprobe`, `userspace/forktest/c_stress/` (`grandfork.c`, `bssfork.c`, `spawnprobe.c`, `killtree.c`), `userspace/ncaprobe`; table in `docs/reference/userspace-layout.md`. Check that the one you pick speaks this kernel's Linux x86_64 ABI before building it.

## Premises (write HOLDS / WRONG / UNCHECKED per row in RESULT-pd.md before your first edit; ~10 calls)
| # | premise | how to check |
|---|---|---|
| 1 | your tree's `taoxichuan/` equals origin `aa1b4cbe` | `git log --oneline -3` shows `b1acc953` |
| 2 | the shell the sshd task spawns can be a registered process for `fork_process` to fork | read what `fork_process` needs of "the calling process" (plan §1 `ExecRuntime`, `RESULT-pc.md` hooks table) - the likely first WRONG |
| 3 | `yes`, `head`, `wc` exist in the guest image | one exec `busybox --list` |
| 4 | the probe you choose speaks this kernel's ABI | its README/source header and the libakuma/musl note in `docs/reference/userspace-layout.md` |
| 5 | the IPI-bound `ThreadRuntime` fields are never called on one core in the park/switch path | plan §1 and the threading crate docs |

## Guest facts (verified; do not rediscover)
One core. No procfs, no `/dev` (no `dd`). The host forward degrades near ~30 connections per boot (a stall looks like a connection accepted but never reaching `[SSH Auth]`: retry once before blaming the kernel). `PACE=15` after every exec.
Ticks: PIT-calibrated LAPIC, vector 32, about 10 ms. The shared `vm-dev.log` keeps older boots' lines: compare line numbers against your boot's `loading akuma-vm guest` line.

## Composition rule (mandatory)
Look first (`grep -rn <name> crates/ amd64/src/`), pull by path dependency, extract from `amd64/src/` into a crate only if it lives only there (new crates `forbid(unsafe_code)`, unsafe budget 0), write new only when it exists nowhere, and say which for each piece. `amd64/` must keep building (`cargo check -p akuma-amd64 --target x86_64-unknown-none --features no-tests`).
`wc -l taoxichuan/src/*.rs` at start and end.

## Gates (extend `scripts/kvm/goose/run_pipes.sh` with a trailing group or add `run_fork.sh` beside it; NOT `run_gates.sh`, which is closed; at most 8 execs per boot)
- F1 through the NEW path: `sh -c 'echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f'` -> exit 0, client bytes `x\n2 /tmp/f\n`.
- P1-P5 (`run_pipes.sh`) pass unchanged through the new path.
- **PK1 (the point of M4):** `sh -c 'yes | head -n 20000 | wc -l'` -> exit 0, client bytes `20000\n` (impossible under M2). If `yes` is missing: `sh -c 'while true; do echo y; done | head -n 20000 | wc -l'`.
- PK2: the probe you pick exits 0 with its own PASS line; zero `Fault|PANIC|sig!` after your boot's start line; ticks still advance.
- Out of scope: timer-driven switching (next slice), ring-0 preemption (never), signals, threads, SMP, deleting the M2 scaffold.
Read client captures with `od -c`; the verdict is the console `[SSH] Exit status` line.

## Ops traps
- Guest kernel installs only with the explicit destination `/boot/akuma-guest-dev`; never write `/boot/akuma-amd64*`.
- `vm-serve-mc` is running and kvm0 is exclusive: stop it before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs; confirm the `guest disk:` line in `/var/log/herd/vm-dev.log`; fresh `.orig` image copy per boot; **restore everything last** (vm-dev disabled, `vm-serve-mc` running).
- Never run python from stdin on this box; write a file and run `python3 /path/x.py < /dev/null`. Do not copy keys or edit ssh/sshd config.
- Debug prints carry `[tmp]` and are listed under "Remove before final" in RESULT-pd.md. The pre-commit hook runs clippy `-D warnings` on every crate for the HOST target: run host clippy on any crate you touch before committing.

## Commit rule (you have no clock)
Commit before every build, and after each piece that builds; write RESULT-pd.md early (premise rows first) and after every build, boot and commit. `git push litter taoxichuan-s4` only if green (host tests pass, kernel builds, no `[tmp]`, no amends of pushed commits). Never push `origin`.

## Budget
About 120 tool calls, 4 boots (a 5th only for a fix). At 50 calls without a commit the supervisor nudges you. If F1 does not pass through the new path after 2 boots, stop and write exactly where it fails (console lines, the step, the hook): that is a valid result.

## When to stop
F1, P1-P5, PK1 and PK2 green, or 3 boots with no new evidence, or a quota error. Write `/root/dev-cycle/taoxichuan-s7/RESULT-pd.md` (premise rows, hooks table real/no-op/stub updated, gates with console evidence, pulled/extracted/written-new, wc -l, what is next) and end the turn.
