# Taoxichuan stage 8, M4 session ph: close slice 3 with clean gate verdicts (F1, P1-P5, PK1; PK2 if cheap), then strip the `[tmp]` prints

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: pg (`/root/dev-cycle/taoxichuan-s7/RESULT-pg.md` and `PLAN-pg.md`; your tree is `8ac51685`, also on `litter` as `taoxichuan-s4`).
`git log --oneline -3` first. **Keep your context small.** Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`.
Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Build: `kbuild -p taoxichuan -j 4 --features no-tests`. Box log times are UTC.

## The goal
Session pg found and fixed the root causes of the writev spin and of the hung sessions (the tick switch now keeps the current-task slot in step with CR3; the exec arm no longer overwrites the spawn crossing's saved frame; a real fork child's output reaches the shell's buffer). F1 is green: `[SSH] Exit status: 0`, client bytes `x\n2 /tmp/f\n`, zero faults, `REAL_FORK` on. What is not known: PK1's verdict after the `/bin/yes` bake, P1-P5 and PK2. The supervisor read the box's capture files during pg's last battery: PK1's client got 38 bytes where `20000\n` was wanted (that boot may have predated the `yes` bake), PK2 exited 0 without its PASS line, and the P battery's clients show "connection lost" with no console exit line. Those are readings of captures, not verdicts; the runner called them UNKNOWN because the console log rotates.
**Close slice 3: with `REAL_FORK` on, F1, P1-P5 and PK1 pass in one clean boot (PK2 if it can be put in the image at reasonable cost), then the `[tmp]` prints are gone and `REAL_FORK` stays on.** Two phases.

## Phase 1 - plan (before any edit or boot)
Copy `PLAN-pg.md` to `PLAN-ph.md`, mark what pg proved, and write what is left: facts with console evidence apart from guesses, hypotheses ordered by cost per bit of information, a fix sequence with a stop condition per step. Commit it. Include, as its own section:
1. **A verdict you can trust, first.** Make the gate runner read verdicts that survive log rotation (drop the rotation-scale prints before the first battery boot, or read rotated logs), so no gate is UNKNOWN. Do this before anything else; nothing else is decidable without it.
2. **The archive and the other kernels, for whatever fails.** For each gate that then fails, `grep -n -i` the error text or symptom in `docs/README.md`, `docs/runbooks/*.md` and `docs/archive/` and write the matching rows, or "no row", in RESULT-ph.md before reading source. The amd64 and aarch64 kernels run the same busybox with pipelines and many ssh connections per boot: find how they serve the case and which crate decides it, and compare with Taoxichuan before you instrument. Pull a crate rather than re-derive its decision.
3. **The connection count.** If clients lose the connection after the first few execs of one boot, say whether the old "channel serves about two connections per boot" symptom is back, and what the console shows at that point.

## Docs to read first (in order; `sed -n` windows of 60 lines or less, never whole files)
1. `RESULT-pg.md` (the two root causes, Gates, Hooks table, Remove before final) and `PLAN-pg.md`.
2. `docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md`.
3. `docs/README.md` symptom matrix; `docs/reference/subsystems/` entries for the scheduler and the console.
Probes already exist: `userspace/forkprobe`, `userspace/forktest/c_stress/`, `userspace/nettest/` (table in `docs/reference/userspace-layout.md`), host tests in the owning crate (`cargo test -p <crate> --target $HOST`).

## Premises (HOLDS / WRONG / UNCHECKED per row in RESULT-ph.md before your first edit; about 6 calls)
| # | premise | how to check |
|---|---|---|
| 1 | your tree is `8ac51685` and builds | `git log --oneline -3`; `kbuild` exit code |
| 2 | the `/bin/yes` bake produces a working `yes` in the guest | one boot, `sh -c 'yes \| head -n 3'` |
| 3 | PK2's cost: pg said "no probe binary, no bake env" | read pg's PK2 note and the bake precedent (`AKUMA_DEVROOT_BAKE_*`); if it costs more than about 15 calls, leave PK2 last and say so |
| 4 | the `[tmp]` prints listed in RESULT-pg "Remove before final" are in the tree | `grep -n "\[tmp\]" taoxichuan/src/*.rs` |

## Guest facts (verified; do not rediscover)
One core. No procfs, no `/dev`. `PACE=15` after every exec. Ticks: PIT-calibrated LAPIC, vector 32, about 10 ms. Preemption switches only from ring 3; kernel paths run IF-masked. `vm-dev.log` rotates at about 250 KB: judge faults from your boot's start line (log mark), never a whole-log grep. `boot_dev.sh` installs the artifact you name: use the default `/root/ktarget/...` path and compare the install line's size/md5 with the fresh build before trusting a boot.

## Composition rule (mandatory)
Look first (`grep -rn <name> crates/ amd64/src/`), pull by path dependency, extract from `amd64/src/` into a crate only if it lives only there (new crates `forbid(unsafe_code)`, unsafe budget 0), write new only when it exists nowhere, and say which for each piece. `amd64/` must keep building (`cargo check -p akuma-amd64 --target x86_64-unknown-none --features no-tests`); gate amd64-only code so the aarch64 build stays green. `wc -l taoxichuan/src/*.rs` at start and end (8029 now). Decisions already made (do not reopen): timer preemption stays in, SMP-ready (slot accessors, no new current-task `static`); ring-0 preemption stays out; keep M2's scaffold compiled and selectable until the new path passes every gate.

## Gates (`scripts/kvm/goose/run_fork.sh` and `run_pipes.sh`; at most 8 execs per boot)
- F1 through the new path: `sh -c 'echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f'` -> console `[SSH] Exit status: 0`, client bytes `x\n2 /tmp/f\n`.
- P1-P5 (`run_pipes.sh`) pass unchanged.
- PK1: `sh -c 'yes | head -n 20000 | wc -l'` -> exit 0, `20000\n`.
- PK2: the probe exits 0 with its own PASS line; zero `Fault|PANIC|sig!` after your boot's start line; ticks still advance.
Read client captures with `od -c`; the verdict is the console `[SSH] Exit status` line AND the client bytes. A gate is green only when both are seen in the same boot, and the file times show they are from that boot.

## Ops traps
- Guest kernel installs only with the explicit destination `/boot/akuma-guest-dev`; never write `/boot/akuma-amd64*`.
- `vm-serve-mc` is running and kvm0 is exclusive: stop it before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs (`boot_dev.sh` does this); fresh `.orig` image copy per boot.
- **Restore everything last and verify it before your final line**: `herd stop vm-dev`, stage4 conf from `/root/dev-cycle/taoxichuan-s6/vm-dev.conf.stage4` into BOTH confs, `herd disable vm-dev`, `herd start vm-serve-mc`, `mirror.sh stop`, then `herd status` (never bare `herd`).
- Never run python from stdin on this box; write a file and run `python3 /path/x.py < /dev/null`. Do not copy keys or edit ssh/sshd config.
- Debug prints carry `[tmp]` and are listed under "Remove before final" in RESULT-ph.md. The pre-commit hook runs clippy `-D warnings` for the HOST target: run host clippy on any crate you touch before committing.

## Commit rule (you have no clock)
Commit before every build and after each piece that builds; write RESULT-ph.md early (premise rows first) and after every build, boot and commit. **Run `git push litter taoxichuan-s4` after every boot cycle that taught you something** (WIP is fine; say so in the message). Never push `origin`. Commit messages say "the user", not "owner".

## Budget
About 120 tool calls and 5 boots; the root causes are found, this session is verdicts and cleanup. If a step yields no new evidence after 2 boots, say so in the plan file, re-plan from what you now know (including another pass over the archive and the other kernels), and continue.

## When to stop
F1, P1-P5 and PK1 green in one clean boot with `REAL_FORK` on by default and no `[tmp]` left (PK2 too if cheap), or 5 boots with no new evidence, or a quota error. Write `/root/dev-cycle/taoxichuan-s7/RESULT-ph.md` (premise rows, hooks table updated, gates with console evidence, pulled/extracted/written-new, wc -l, what is next), restore ops, and end the turn.
