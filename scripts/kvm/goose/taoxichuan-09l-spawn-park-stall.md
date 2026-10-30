# Taoxichuan stage 8, session pl: the spawn-park stall - plan first, then find the lost wake

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: pj (`/root/dev-cycle/taoxichuan-s7/RESULT-pj.md`, `PLAN-pj.md`; its tip `bc626f98` is on `litter` as `taoxichuan-s4`; the supervisor's merge into the experiment branch added `taoxichuan/docs/`, which you may not have: `git pull --ff-only litter taoxichuan-s4` if the supervisor has pushed it, otherwise read `taoxichuan/docs/` on the supervisor's say-so later and go on without it).
`git log --oneline -3` first. **Keep your context small.** Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`.
Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Build: `kbuild -p taoxichuan -j 4 --features no-tests`. Box log times are UTC.

## The goal
**One defect, and nothing else this session: in roughly one boot in four, the guest parks mid-spawn and nothing ever wakes it.** Every later stage (`git clone`, `cargo fetch`, `rustc`) is many execs in one boot, so an unreliable boot costs more than any feature. The user's decision: fix it now, before anything else.
What is known (facts, from session pj's RESULT "Stall-rate statement" and "What is next", and from the supervisor's own probes):
- Shape: `[SSH] Exec:` is printed, then `[TICK pre]`, then cpu0 is halted forever; the tick is alive (about 200 exits/s), `hlt=99%`, relay counters frozen (`gw_frames_from_guest` constant, `to_guest` constant), `blk=0 io=0`, no DNS or outbound activity. The client side shows `connection stalled during handshake` and reconnect attempts. The spawn's next step never runs and nothing wakes it.
- It is NOT the idle spin (idle halts correctly now), NOT the relay starving (the relay has the core), NOT ph's five wait paths (they all halt).
- Rate: pj saw 2 of 7 boots (one after two green execs, one at the first exec). The supervisor saw it on the **first real exec after the boot-ok exec** on a fresh boot (`apk add git`); the guest console and relay log of that boot are saved on the box as `/root/dev-cycle/taoxichuan-s7/STALL-sup1.vm-dev.log` (`STALL-sup2.vm-dev.log` and `STALL-sup3.vm-dev.log` are the other two: judge them yourself). Read these before any new boot: they are free evidence.
- **A near-deterministic trigger exists**: `apk add git` over ssh on a fresh boot (the frozen mirror at `/srv/alpine-mirror` now carries git, libexpat and pcre2 besides the curl closure; `sh scripts/kvm/goose/mirror.sh start`, then `boot_dev.sh`, then `/bin/ssh -p 2224 root@localhost 'apk add git'` from the box) stalled in 2 of 3 fresh boots (`STALL-sup1` and `STALL-sup3` on the box, `STALL-sup2` passed with exit 0 and no client output). Use it as the harness's heavy exec: a fix that survives only the cheap gates has not fixed this. Start the mirror before the harness runs it and stop it after.
- An exec that stalls takes the whole sshd channel with it: later execs on the same boot fail too (so a stall costs the whole battery, which is why this blocks everything).
A fix is a cause you can name from evidence, a change at that site, and a stall rate you can state over many boots.

## Phase 1 - plan (before any edit or boot)
Write `PLAN-pl.md` and commit it. It must contain, from reading and not from guessing:
1. **A stall-rate harness first.** A script (beside `run_fork.sh`, same conventions) that boots, runs a fixed 3-exec sequence (the three cheapest gates), classifies the boot as CLEAN / STALL by the console exit lines **per gate window** (the verdict window starts at that gate's own start, not at the boot's: a whole-boot window sees an earlier gate's `Exit status` line and misreads every later gate), kills nothing early, saves each stalled boot's console slice, and prints `clean/total` over N boots in one command. Cost per boot is about 1 minute (`boot_dev.sh` ready in ~50 s). Everything below is measured with it. Its first output is the baseline rate on the unmodified tree.
2. **Where wakes come from.** The spawn parks the sshd task; something must resume it. Name the wake source for every park in the spawn/exec path (`spawn_arm`, the crossing, `exec_real_task`, `wait4_real`, `task_exit_real`, the bridge) from the source and from `taoxichuan/docs/reference/processes.md` if you have it. Then find how the amd64 and aarch64 kernels wake a parked task in the same situation (which crate decides, what the wake is keyed on) and what the archive calls the lost-wakeup signature: `grep -n -i "lost wake\|lost-wake\|missed wake\|wake.*before.*park\|park.*before.*wake" docs/archive/*.md docs/runbooks/*.md docs/README.md`, write the matching rows (or "no row") before you read more source. The classic shape is a wake delivered before the park is armed.
3. **Hypotheses ordered by cost per bit of information**, each with the one-boot instrument or the host test that would confirm or kill it, and a stop condition. A host test of the owning crate's park/wake decision (`cargo test -p <crate> --target $HOST`) is preferred over a boot when the decision lives in a crate.
Then carry it out as Phase 2, noting changes in the file rather than rewriting it silently.

## Docs to read first (in order; `sed -n` windows of 60 lines or less)
1. `RESULT-pj.md` (the Stall-rate statement, "What is next"), `PLAN-pj.md`, `RESULT-ph.md` (its stall notes), `docs/archive/TAOXICHUAN_M4_PI_STALL_PLAN.md`.
2. The saved stalled boots (above), and `docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md` for how a spawn is meant to complete.
3. `docs/README.md` symptom matrix; `docs/reference/subsystems/` entries for the scheduler and IRQ.

## Guest facts (verified; do not rediscover)
One core. `PACE=15` after every exec (a shorter pace changes the stall rate: if you change it, say so). Preemption switches only from ring 3; kernel paths run IF-masked. The host relay thread shares the pinned host core with the vCPU. `vm-dev.log` rotates at about 250 KB: judge from your boot's own window. `boot_dev.sh` installs the artifact you name: use the default `/root/ktarget/...` path and compare the install line's md5 with the fresh build.

## Composition rule (mandatory)
Look first (`grep -rn <name> crates/ amd64/src/`), pull by path dependency, extract from `amd64/src/` into a crate only if it lives only there (new crates `forbid(unsafe_code)`, unsafe budget 0), write new only when it exists nowhere, and say which for each piece. `amd64/` must keep building (`cargo check -p akuma-amd64 --target x86_64-unknown-none --features no-tests`); gate amd64-only code so aarch64 stays green. `wc -l taoxichuan/src/*.rs` at start and end. Decisions already made (do not reopen): timer preemption stays, SMP-ready, no ring-0 preemption, M2's scaffold stays compiled and selectable.

## Gates
- Stability: the harness reports `N clean / N boots` for N >= 10 consecutive on the final tree. If one stalls, that is the finding, not a flake: save its slice and iterate.
- The already-green gates stay green: one full battery at the end (`run_fork.sh` then `run_pipes.sh`: F1, PK1, PK2, P1-P5, exact client bytes read with `od -c` and the console `[SSH] Exit status`, same boot).

## Docs
Update `taoxichuan/docs/` if you have it (`reference/processes.md`, the process-model diagram, `gaps.md`) in the same commits as the code; if you do not have it yet, write the mechanism into `RESULT-pl.md` in a section titled "For the docs".

## Ops traps
- Guest kernel installs only to `/boot/akuma-guest-dev`; never write `/boot/akuma-amd64*`.
- `vm-serve-mc` is running and kvm0 is exclusive: stop it before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs (`boot_dev.sh` does this); fresh `.orig` image copy per boot.
- **Restore everything last and verify it before your final line**: `herd stop vm-dev`, stage4 conf from `/root/dev-cycle/taoxichuan-s6/vm-dev.conf.stage4` into BOTH confs, `herd disable vm-dev`, `herd start vm-serve-mc`, `mirror.sh stop`, then `herd status` (never bare `herd`).
- Never run python from stdin on this box; write a file and run `python3 /path/x.py < /dev/null`. Do not copy keys or edit ssh/sshd config.
- Debug prints carry `[tmp]` and are listed under "Remove before final" in RESULT-pl.md. The pre-commit hook runs clippy `-D warnings` for the HOST target: run host clippy on any crate you touch before committing.

## Commit rule (you have no clock)
Commit before every build and after each piece that builds; write RESULT-pl.md early (premise rows first) and after every build, boot and commit. **Run `git push litter taoxichuan-s4` after every boot cycle that taught you something.** Never push `origin`. Commit messages say "the user", not "owner".

## Budget
About 100 tool calls and 12 boots (the harness runs are boots; run it in one command, not one call per boot). If a step yields no new evidence after 2 boots, say so in the plan file, re-plan from what you now know (another pass over the archive and the other kernels), and continue.

## When to stop
10 consecutive clean boots on the final tree and one full green battery, or 12 boots with no new evidence, or a quota error. Write `/root/dev-cycle/taoxichuan-s7/RESULT-pl.md` (premise rows, the mechanism with console evidence, the baseline and final stall rates, the harness's name, pulled/extracted/written-new, wc -l, what is next), restore ops, and end the turn.
