# Taoxichuan stage 8, M4 session pf: slice 3 gates through the new path (F1, P1-P5, PK1, PK2 with `REAL_FORK` on)

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: pe (`/root/dev-cycle/taoxichuan-s7/RESULT-pe.md` and `PLAN-pe.md`; your tree is `983dc1c5`, also on `litter` as `taoxichuan-s4`).
`git log --oneline -3` first. **Keep your context small.** Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`.
Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Build: `kbuild -p taoxichuan -j 4 --features no-tests`. Box log times are UTC.

## The goal
Session pe proved the process model's core on three boots: a forked child is a real task, `wait4` parks the parent, the child's exit wakes it, the switch lands back on the shell, and the reap returns the child's pid, with zero faults. What it did not get is the gate verdicts through the new path: F1's client bytes and exit status, P1-P5, PK1 and PK2.
**Close M4 slice 3: with `REAL_FORK` on, F1, P1-P5 and PK1 (`yes | head -n 20000 | wc -l` -> `20000\n`) pass in a clean boot, and PK2 (the probe) passes if it can be put in the image at reasonable cost.** Then remove the `[tmp]` prints and leave `REAL_FORK` on by default.
This is the experiment's crux: a small open model assembling a kernel with a real process model. You found and fixed two root causes last session from evidence; the same method applies. Two phases again.

## Phase 1 - update the plan (before any edit or boot)
Copy `PLAN-pe.md` to `PLAN-pf.md`, mark what pe proved, and write what is left: what you know (facts with console evidence, apart from guesses), hypotheses ordered by cost per bit of information, the fix sequence with a stop condition per step, and which amd64 archive docs apply. Commit it. Read the docs and source before writing it. Then carry it out as Phase 2, noting changes in the file rather than rewriting it silently.

## Docs to read first (in order; `sed -n` windows of 60 lines or less, never whole files)
1. `RESULT-pe.md` (the sections "Boots C-G" and "Boots H-I close-out", "Hooks table") and `PLAN-pe.md` §§ 1, 3, 4 - your own record.
2. `docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md` - the spec; the slice list says what comes after slice 3.
3. The docs `PLAN-pe.md` § 4 names for blocking syscalls, wait queues and the lost-wakeup signature.
4. `docs/README.md` symptom matrix; `docs/reference/subsystems/` entries for the scheduler and the console.
**Probes already exist - use one before writing a new one**: `userspace/forkprobe`, `userspace/forktest/c_stress/`, `userspace/nettest/` (table in `docs/reference/userspace-layout.md`), host tests in the crate that owns a decision (`cargo test -p <crate> --target $HOST`). When a gate fails, `grep -n -i "<error text>" docs/README.md docs/runbooks/*.md` BEFORE reading source, and write the matching rows (or "no row") in RESULT-pf.md.

## Premises (write HOLDS / WRONG / UNCHECKED per row in RESULT-pf.md before your first edit; about 10 calls)
| # | premise | how to check |
|---|---|---|
| 1 | your tree is `983dc1c5` and builds | `git log --oneline -3`; `kbuild` exit code |
| 2 | `REAL_FORK` is `true` and the `[tmp]` prints listed in RESULT-pe "Remove before final" are in the tree | `grep -n "REAL_FORK\|\[tmp\]" taoxichuan/src/*.rs` |
| 3 | RESULT-pe says the ssh channel serves about two connections per boot and then stalls | check whether it also holds with `REAL_FORK` off (boot B ran 8+ spawns on one boot without a wedge) and with it on |
| 4 | the pipe arms still drop wakes and never park | read what `RESULT-pe.md` says and what the arms do now |
| 5 | the probe you pick for PK2 can get into the guest image | find how the image is built (`docs/README.md`, `scripts/kvm/goose/boot_dev.sh`, `grow_dev_image.sh`, `refroot.sh`); if it costs more than about 15 calls, put PK2 last and say so |

## Guest facts (verified; do not rediscover)
One core. No procfs, no `/dev` (no `dd`). `PACE=15` after every exec. Ticks: PIT-calibrated LAPIC, vector 32, about 10 ms.
`vm-dev.log` rotates at about 250 KB and keeps older boots' lines: judge faults from your boot's start line (log mark), never a whole-log grep. `boot_dev.sh` installs the artifact you name: use the default `/root/ktarget/...` path and compare the install line's size/md5 with the fresh build before trusting a boot.

## Composition rule (mandatory)
Look first (`grep -rn <name> crates/ amd64/src/`), pull by path dependency, extract from `amd64/src/` into a crate only if it lives only there (new crates `forbid(unsafe_code)`, unsafe budget 0), write new only when it exists nowhere, and say which for each piece. `amd64/` must keep building (`cargo check -p akuma-amd64 --target x86_64-unknown-none --features no-tests`).
`wc -l taoxichuan/src/*.rs` at start and end (7618 now). Decisions already made (do not reopen): timer preemption is the NEXT slice (switch only where a task parks, never in ring 0); SMP-ready (slot accessors, no new current-task `static`); pull crates, do not copy code; keep M2's scaffold compiled and selectable until the new path passes every gate.

## Gates (extend `scripts/kvm/goose/run_pipes.sh` with a trailing group or add `run_fork.sh` beside it; NOT `run_gates.sh`; at most 8 execs per boot)
- F1 through the NEW path: `sh -c 'echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f'` -> console `[SSH] Exit status: 0`, client bytes `x\n2 /tmp/f\n`.
- P1-P5 (`run_pipes.sh`) pass unchanged through the new path.
- PK1: `sh -c 'yes | head -n 20000 | wc -l'` -> exit 0, `20000\n`.
- PK2: the probe exits 0 with its own PASS line; zero `Fault|PANIC|sig!` after your boot's start line; ticks still advance.
- Out of scope: timer-driven switching, ring-0 preemption, signals beyond what a gate needs, threads, SMP, deleting the M2 scaffold.
Read client captures with `od -c`; the verdict is the console `[SSH] Exit status` line.

## Ops traps
- Guest kernel installs only with the explicit destination `/boot/akuma-guest-dev`; never write `/boot/akuma-amd64*`.
- `vm-serve-mc` is running and kvm0 is exclusive: stop it before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs (`boot_dev.sh` does this); confirm the `guest disk:` line; fresh `.orig` image copy per boot.
- **Restore everything last and verify it before your final line**: `herd stop vm-dev`, stage4 conf from `/root/dev-cycle/taoxichuan-s6/vm-dev.conf.stage4` into BOTH confs, `herd disable vm-dev`, `herd start vm-serve-mc`, `mirror.sh stop`, then `herd status` (never bare `herd`: it starts a second supervisor).
- Never run python from stdin on this box; write a file and run `python3 /path/x.py < /dev/null`. Do not copy keys or edit ssh/sshd config.
- Debug prints carry `[tmp]` and are listed under "Remove before final" in RESULT-pf.md. The pre-commit hook runs clippy `-D warnings` for the HOST target: run host clippy on any crate you touch before committing.

## Commit rule (you have no clock)
Commit before every build and after each piece that builds; write RESULT-pf.md early (premise rows first) and after every build, boot and commit. **Run `git push litter taoxichuan-s4` after every boot cycle that taught you something** (WIP is fine; say so in the message): last session never pushed, and the branch on `litter` is what survives a box reset. Never push `origin`.

## Budget
About 160 tool calls and 6 boots, with a long-session cost per call in mind (a fresh context with a short continuation is cheaper than a long one). If a step yields no new evidence after 2 boots, say so in the plan file, re-plan from what you now know, and continue.

## When to stop
F1, P1-P5 and PK1 green in a clean boot with `REAL_FORK` on by default and no `[tmp]` left (PK2 too if its image cost was reasonable), or 6 boots with no new evidence, or a quota error. Write `/root/dev-cycle/taoxichuan-s7/RESULT-pf.md` (premise rows, hooks table updated, gates with console evidence, pulled/extracted/written-new, wc -l, what is next), restore ops, and end the turn.
