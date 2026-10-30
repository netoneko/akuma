# Taoxichuan stage 8, M4 session pg: the exec'd shell's writev loop, then slice 3 gates (F1, P1-P5, PK1, PK2 with `REAL_FORK` on)

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: pf (`/root/dev-cycle/taoxichuan-s7/RESULT-pf.md` and `PLAN-pf.md`; your tree is `15cc24e0` on `taoxichuan-s4`, also on `litter`).
`git log --oneline -3` first. **Keep your context small.** Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`.
Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Build: `kbuild -p taoxichuan -j 4 --features no-tests`. Box log times are UTC.

## The goal
Session pf built timer-driven preemption (SMP-ready, ring-3 only) and proved from console evidence that the old "~2 connections per boot" stall was a ring-3 spin, not the channel. With preemption armed the box now stays alive: sshd is up, the tick switch rotates tasks, a single-command exec completes with `[SSH] Exit status: 0`, zero faults. What remains, per RESULT-pf "What is next": the exec'd shell still spins in musl's `writev` retry loop (the guest busybox, rip `0x4ceaa1`), so F1's multi-command exec and PK1 do not finish, and no gate verdict has been read cleanly since the console log started rotating.
**Close M4 slice 3: with `REAL_FORK` on, F1, P1-P5 and PK1 pass in a clean boot, PK2 if it is cheap; then remove the `[tmp]` prints and leave `REAL_FORK` on.** Two phases.

## Phase 1 - plan (before any edit or boot)
Copy `PLAN-pf.md` to `PLAN-pg.md`, mark what pf proved, and write what is left: facts with console evidence apart from guesses, hypotheses ordered by cost per bit of information, a fix sequence with a stop condition per step. Commit it. Phase 1 must include, written into the plan as its own section:
1. **The archive and the other kernels first.** `grep -n -i` `docs/archive/`, `docs/runbooks/`, `docs/README.md` for the symptom words (writev, `__stdio_write`, short write, EAGAIN on a pipe or the console, a ring-3 spin on write, a nested shell's stdout) and for the fd kinds involved; write the matching rows, or "no row", in RESULT-pg.md. The amd64 kernel runs this same busybox with nested shells and pipelines, and the aarch64 kernel too: find how each answers `write`/`writev` on a pipe, on the console and on a spawned child's stdio, and which crate owns the decision. Compare that with what Taoxichuan's arms do for the same fd kind, and say in the plan whether the difference explains the loop **before** you add an instrument. Pull the crate if one decides it; do not re-derive it.
2. **A readable gate runner.** pf's gate verdicts were all UNKNOWN because the `sw to=` print rotates `vm-dev.log` every ~30 s. Decide how the runner reads a verdict reliably (drop the rotation-scale print first; consider reading rotated logs) and do that before the first gate boot.

## Docs to read first (in order; `sed -n` windows of 60 lines or less, never whole files)
1. `RESULT-pf.md` (hand-off, Gates final, Hooks table) and `PLAN-pf.md` §5.
2. `docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md` and `docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md`.
3. `docs/README.md` symptom matrix; `docs/reference/subsystems/` entries for the scheduler, SMP, console and tty.
Probes already exist: `userspace/forkprobe`, `userspace/forktest/c_stress/`, `userspace/nettest/` (table in `docs/reference/userspace-layout.md`), host tests in the owning crate (`cargo test -p <crate> --target $HOST`).

## Premises (HOLDS / WRONG / UNCHECKED per row in RESULT-pg.md before your first edit; about 8 calls)
| # | premise | how to check |
|---|---|---|
| 1 | your tree is `15cc24e0` and builds | `git log --oneline -3`; `kbuild` exit code |
| 2 | the loop is a `writev` whose fd 1 is not served by a pipe arm | RESULT-pf hand-off; the WRITEV kind match in `taoxichuan/src/` |
| 3 | the amd64 kernel serves the same case through a shared crate | the archive grep in Phase 1 |
| 4 | `REAL_FORK` is true and the `[tmp]` prints listed in RESULT-pf "Remove before final" are in the tree | `grep -n "REAL_FORK\|\[tmp\]" taoxichuan/src/*.rs` |

## Guest facts (verified; do not rediscover)
One core. No procfs, no `/dev`. `PACE=15` after every exec. Ticks: PIT-calibrated LAPIC, vector 32, about 10 ms. Preemption switches only from ring 3; kernel paths run IF-masked. `vm-dev.log` rotates at about 250 KB: judge faults from your boot's start line (log mark), never a whole-log grep. `boot_dev.sh` installs the artifact you name: use the default `/root/ktarget/...` path and compare the install line's size/md5 with the fresh build before trusting a boot.

## Composition rule (mandatory)
Look first (`grep -rn <name> crates/ amd64/src/`), pull by path dependency, extract from `amd64/src/` into a crate only if it lives only there (new crates `forbid(unsafe_code)`, unsafe budget 0), write new only when it exists nowhere, and say which for each piece. `amd64/` must keep building (`cargo check -p akuma-amd64 --target x86_64-unknown-none --features no-tests`). Gate amd64-only code and crates (cfg, keep out of `default-members`) so the aarch64 build stays green. `wc -l taoxichuan/src/*.rs` at start and end (7870 now). Decisions already made (do not reopen): timer preemption stays in, SMP-ready (slot accessors, no new current-task `static`); ring-0 preemption stays out; pull crates, do not copy code; keep M2's scaffold compiled and selectable until the new path passes every gate.

## Gates (`scripts/kvm/goose/run_fork.sh` and `run_pipes.sh`; at most 8 execs per boot)
- F1 through the new path: `sh -c 'echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f'` -> console `[SSH] Exit status: 0`, client bytes `x\n2 /tmp/f\n`.
- P1-P5 (`run_pipes.sh`) pass unchanged.
- PK1: `sh -c 'yes | head -n 20000 | wc -l'` -> exit 0, `20000\n`.
- PK2: the probe exits 0 with its own PASS line; zero `Fault|PANIC|sig!` after your boot's start line; ticks still advance.
Read client captures with `od -c`; the verdict is the console `[SSH] Exit status` line, never a runner's UNKNOWN taken as green or red.

## Ops traps
- Guest kernel installs only with the explicit destination `/boot/akuma-guest-dev`; never write `/boot/akuma-amd64*`.
- `vm-serve-mc` is running and kvm0 is exclusive: stop it before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs (`boot_dev.sh` does this); fresh `.orig` image copy per boot.
- **Restore everything last and verify it before your final line**: `herd stop vm-dev`, stage4 conf from `/root/dev-cycle/taoxichuan-s6/vm-dev.conf.stage4` into BOTH confs, `herd disable vm-dev`, `herd start vm-serve-mc`, `mirror.sh stop`, then `herd status` (never bare `herd`).
- Never run python from stdin on this box; write a file and run `python3 /path/x.py < /dev/null`. Do not copy keys or edit ssh/sshd config.
- Debug prints carry `[tmp]` and are listed under "Remove before final" in RESULT-pg.md. The pre-commit hook runs clippy `-D warnings` for the HOST target: run host clippy on any crate you touch before committing.

## Commit rule (you have no clock)
Commit before every build and after each piece that builds; write RESULT-pg.md early (premise rows first) and after every build, boot and commit. **Run `git push litter taoxichuan-s4` after every boot cycle that taught you something** (WIP is fine; say so in the message). Never push `origin`. Commit messages say "the user", not "owner".

## Budget
About 160 tool calls and 6 boots. If a step yields no new evidence after 2 boots, say so in the plan file, re-plan from what you now know (including another pass over the archive and the other kernels), and continue.

## When to stop
F1, P1-P5 and PK1 green in a clean boot with `REAL_FORK` on by default and no `[tmp]` left (PK2 too if cheap), or 6 boots with no new evidence, or a quota error. Write `/root/dev-cycle/taoxichuan-s7/RESULT-pg.md` (premise rows, hooks table updated, gates with console evidence, pulled/extracted/written-new, wc -l, what is next), restore ops, and end the turn.
