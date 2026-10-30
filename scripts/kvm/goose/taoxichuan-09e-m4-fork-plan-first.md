# Taoxichuan stage 8, M4 session pe: slice 3 again - PLAN the fix first, then carry it out (gate: `yes | head | wc` runs concurrently)

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: pd (`/root/dev-cycle/taoxichuan-s7/RESULT-pd.md`; its five commits `437a898e`..`33378e5d` are your starting checkpoint, also on `litter` as branch `taoxichuan-pd-wip`).
**Keep your context small.** Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`. `git log --oneline -6` first: it should end at `33378e5d`.
Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Build: `kbuild -p taoxichuan -j 4 --features no-tests`. Box log times are UTC.

## The goal
Unchanged from pd: today `fork` is M2's blocking scaffold (the child runs to completion inside the `fork` syscall), so `yes | head` or `git clone`'s helper pump can never work. The amd64 kernel already runs the real thing through shared crates.
**Make taoxichuan do the same: a forked child is a real task, `wait4` parks the parent until the child exits, a pipe reader/writer parks when it must and is woken by the other end, and the scheduler switches between tasks at those park points.**
This is the experiment's crux: a small open model assembling a kernel with a real process model. Pd got `fork_process` running and wrote the switch, but F1 through the new path is not green. This session is a debugging session, so it has two phases.

## Phase 1 - write the plan (before any edit or boot)
Write `/root/dev-cycle/taoxichuan-s7/PLAN-pe.md` and commit it. It must contain:
1. **What is known**: from `RESULT-pd.md` and the pd commits, the facts with their console evidence, kept apart from guesses.
2. **Hypotheses**, each with: what it predicts, the cheapest way to confirm or kill it (a host test, a disassembly, an existing probe, a reading of how the amd64 kernel does the same step, one boot), and its cost. Order them by cost per bit of information.
3. **The fix sequence**: the order in which you will test and fix, with the stop condition of each step.
4. **What from the amd64 kernel's history applies**: which archive docs describe bugs of this shape (fork, context switch, first entry to ring 3, stack and crossing state) and what they say.
Read the docs and the source before you write it; the plan is judged on how well it uses what the repo already knows. Then carry it out as Phase 2, updating the plan file when evidence changes it (note the change, do not silently rewrite).

## Docs to read first (in order; `sed -n` windows of 60 lines or less, never whole files)
1. `docs/README.md` - the front door and symptom matrix.
2. `docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md` - the spec for this slice (hook tables, where the switch happens, stacks vs crossing statics, slice list).
3. `docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md` - what amd64 consumes and the unknowns.
4. `RESULT-pd.md`, `RESULT-pc.md`, and the agent log sections "Stage 8, session pc" and "session pd" in `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`.
5. The amd64 fork and context-switch history: `grep -il "fork_process\|switch_to\|context switch\|first entry" docs/archive/*.md docs/reference/subsystems/*.md`, then read the ones that match by title.
6. `docs/reference/subsystems/` entries for the scheduler, SMP and exceptions; `docs/runbooks/` for any runbook on kernel faults (`rip=0`, nested fault, trace of syscalls).
**Probes already exist - use one before writing a new one**: `userspace/forkprobe`, `userspace/forktest/c_stress/` (`grandfork.c`, `bssfork.c`, `spawnprobe.c`, `killtree.c`), `userspace/abiprobe`; table in `docs/reference/userspace-layout.md`. Host tests in the crates that own the logic (`cargo test -p <crate> --target $HOST`) are cheaper than any boot: a decision that can be a host test should be one.
When a gate fails, `grep -n -i "<error text>" docs/README.md docs/runbooks/*.md` BEFORE reading source, and write the matching rows (or "no row") in RESULT-pe.md.

## Premises (write HOLDS / WRONG / UNCHECKED per row in RESULT-pe.md before your first edit; about 10 calls)
| # | premise | how to check |
|---|---|---|
| 1 | your tree ends at pd's `33378e5d` and builds | `git log --oneline -3`; `kbuild -p taoxichuan -j 4 --features no-tests` exit code |
| 2 | `REAL_FORK` is `true` in that tree and two `[tmp]` prints exist | `grep -n "REAL_FORK\|\[tmp\]" taoxichuan/src/*.rs` |
| 3 | `yes`, `head`, `wc` exist in the guest image | one exec `busybox --list` |
| 4 | the probe you pick speaks this kernel's ABI | its source header and `docs/reference/userspace-layout.md` |
| 5 | M2 (blocking fork) is still green when `REAL_FORK` is off | F1 and P1-P5 on one boot with the flag off |

## Guest facts (verified; do not rediscover)
One core. No procfs, no `/dev` (no `dd`). The host forward degrades near ~30 connections per boot (a stall looks like a connection accepted but never reaching `[SSH Auth]`: retry once before blaming the kernel). `PACE=15` after every exec.
Ticks: PIT-calibrated LAPIC, vector 32, about 10 ms. `vm-dev.log` keeps older boots' lines: compare line numbers against your boot's `loading akuma-vm guest` line.

## Composition rule (mandatory)
Look first (`grep -rn <name> crates/ amd64/src/`), pull by path dependency, extract from `amd64/src/` into a crate only if it lives only there (new crates `forbid(unsafe_code)`, unsafe budget 0), write new only when it exists nowhere, and say which for each piece. `amd64/` must keep building (`cargo check -p akuma-amd64 --target x86_64-unknown-none --features no-tests`).
`wc -l taoxichuan/src/*.rs` at start and end. Decisions already made (do not reopen): timer preemption is the NEXT slice (switch only where a task parks, never in ring 0); SMP-ready (no new `static` current task, go through the slot accessors); pull crates, do not copy code; keep M2's scaffold compiled and selectable until the new path passes every gate, then flip the default and leave deleting for the next session.

## Gates (extend `scripts/kvm/goose/run_pipes.sh` with a trailing group or add `run_fork.sh` beside it; NOT `run_gates.sh`; at most 8 execs per boot)
- F1 through the NEW path: `sh -c 'echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f'` -> exit 0, client bytes `x\n2 /tmp/f\n`.
- P1-P5 (`run_pipes.sh`) pass unchanged through the new path.
- **PK1 (the point of M4):** `sh -c 'yes | head -n 20000 | wc -l'` -> exit 0, client bytes `20000\n`. If `yes` is missing: `sh -c 'while true; do echo y; done | head -n 20000 | wc -l'`.
- PK2: the probe you pick exits 0 with its own PASS line; zero `Fault|PANIC|sig!` after your boot's start line; ticks still advance.
- Out of scope: timer-driven switching, ring-0 preemption, signals, threads, SMP, deleting the M2 scaffold.
Read client captures with `od -c`; the verdict is the console `[SSH] Exit status` line.

## Ops traps
- Guest kernel installs only with the explicit destination `/boot/akuma-guest-dev`; never write `/boot/akuma-amd64*`.
- `vm-serve-mc` is running and kvm0 is exclusive: stop it before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs (`boot_dev.sh` does this); confirm the `guest disk:` line in `/var/log/herd/vm-dev.log`; fresh `.orig` image copy per boot.
- **Restore everything last, and verify it before you write the final line**: `herd stop vm-dev`, stage4 conf from `/root/dev-cycle/taoxichuan-s6/vm-dev.conf.stage4` into BOTH confs, `herd disable vm-dev`, `herd start vm-serve-mc`, `mirror.sh stop`; then `herd status` (never bare `herd`: it starts a second supervisor).
- Never run python from stdin on this box; write a file and run `python3 /path/x.py < /dev/null`. Do not copy keys or edit ssh/sshd config.
- Debug prints carry `[tmp]` and are listed under "Remove before final" in RESULT-pe.md. The pre-commit hook runs clippy `-D warnings` on every crate for the HOST target: run host clippy on any crate you touch before committing.

## Commit rule (you have no clock)
Commit before every build and after each piece that builds; write RESULT-pe.md early (premise rows first) and after every build, boot and commit, so a dead stream or a quota stop loses nothing. Push to `litter taoxichuan-s4` after every boot cycle that taught you something (WIP is fine; say so in the message). Never push `origin`.

## Budget
About 160 tool calls and 6 boots. At 60 calls without a commit the supervisor nudges you. If a step of the plan yields no new evidence after 2 boots, say so in the plan file, re-plan from what you now know, and continue; stop only at the end condition below.

## When to stop
F1, P1-P5, PK1 and PK2 green in a clean boot with `REAL_FORK` default on and no `[tmp]` left, or 6 boots with no new evidence, or a quota error. Write `/root/dev-cycle/taoxichuan-s7/RESULT-pe.md` (premise rows, hooks table real/no-op/stub updated, gates with console evidence, pulled/extracted/written-new, wc -l, what is next) and end the turn.
