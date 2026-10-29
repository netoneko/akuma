# Taoxichuan stage 8 proper, session pk: `git clone` over HTTPS in the guest

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: pj (`/root/dev-cycle/taoxichuan-s7/RESULT-pj.md`, `PLAN-pi.md`; read `git log --oneline -8` for your tree, branch `taoxichuan-s4`, also on `litter`).
**Keep your context small.** Checkout `/src/github.com/netoneko/akuma`. Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Build: `kbuild -p taoxichuan -j 4 --features no-tests`. Box log times are UTC.

## The goal
The ladder's stage 8: **`git clone` over HTTPS works in the Taoxichuan guest; `git log` and `git fsck` agree with a host-side clone of the same repo.** The process model (real fork, `wait4`, pipes, timer preemption) is done as far as F1, PK1, P1-P5 go; stage 7 (`curl -sS https://example.com`) is done. git forks a transport helper (`git-remote-https`) and pumps it through pipes, the shape the process model was built for.
Pick a small fixed public repo and a fixed ref (write it in the plan, with its expected `git log --oneline | wc -l` and HEAD id taken from a host-side clone). Gate: console `[SSH] Exit status: 0` for the clone, then `git log --oneline | wc -l` and `git rev-parse HEAD` equal the host's, and `git fsck` exits 0, all in the same boot; zero `Fault|PANIC|sig!` in the boot's own window.
Two phases.

## Phase 1 - plan (before any edit or boot)
Write `PLAN-pk.md` and commit it. It must contain, from reading and not from guessing:
1. **How git gets into the image.** Stage 6 did `apk add curl` (see the agent log's stage 6 and 9 sections); find how, and what `apk add git` adds (packages, dynamic libraries, helper binaries). Cost it in boots.
2. **The known syscall gaps.** `docs/archive/GIT_MISSING_SYSCALLS.md` (704 lines; read it in windows) lists what git needed on the aarch64 kernel before clone worked, and `docs/archive/GIT_CLONE_STALE_ITIMER_SIGALRM.md` another. For each item say whether Taoxichuan already answers it (grep the arms and the crates), is missing it, or it does not apply. Rank the missing ones by how early git needs them.
3. **The amd64 and aarch64 precedent.** How those kernels serve the same syscalls, and which crate decides each; pull a crate's decision rather than re-derive it (composition rule below).
4. **Premises** (HOLDS / WRONG / UNCHECKED, about 6 calls): the tree builds and the F1/PK1/P1-P5 battery is still green; `/dev/null` is available to a guest process (earlier notes say the guest has no `/dev`); the guest can resolve and reach the clone host; the image has room for git (disk size); `git --version` runs.
Then run: one boot per missing item, from the first failure, reading the console and `docs/README.md` symptom matrix and archive for the error text BEFORE source.

## Docs to read first (in order; `sed -n` windows of 60 lines or less)
1. `RESULT-pj.md`, `docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md` (M1, M2 and what is said about git's helper pump), `docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md`.
2. The two git docs above; `docs/README.md` symptom matrix.
3. The agent log's sections on stage 7 (curl HTTPS) for the network path that already works.

## Guest facts (verified; do not rediscover)
One core. `PACE=15` after every exec. Ticks: PIT-calibrated LAPIC, about 10 ms. Preemption switches only from ring 3. `vm-dev.log` rotates at about 250 KB: judge faults from your boot's own window. `boot_dev.sh` installs the artifact you name: use the default `/root/ktarget/...` path and compare the install line's size/md5 with the fresh build. A stall can occur on a boot (see RESULT-pj for the current rate); if a boot stalls before the first exec, say so and re-boot rather than diagnosing git.

## Composition rule (mandatory)
Look first (`grep -rn <name> crates/ amd64/src/`), pull by path dependency, extract from `amd64/src/` into a crate only if it lives only there (new crates `forbid(unsafe_code)`, unsafe budget 0), write new only when it exists nowhere, and say which for each piece. `amd64/` must keep building (`cargo check -p akuma-amd64 --target x86_64-unknown-none --features no-tests`); gate amd64-only code so aarch64 stays green. `wc -l taoxichuan/src/*.rs` at start and end. Decisions already made (do not reopen): timer preemption stays; SMP-ready; no ring-0 preemption; M2's scaffold stays selectable.

## Ops traps
- Guest kernel installs only to `/boot/akuma-guest-dev`; never write `/boot/akuma-amd64*`.
- `vm-serve-mc` is running and kvm0 is exclusive: stop it before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs (`boot_dev.sh` does this); fresh `.orig` image copy per boot.
- **Restore everything last and verify it before your final line**: `herd stop vm-dev`, stage4 conf from `/root/dev-cycle/taoxichuan-s6/vm-dev.conf.stage4` into BOTH confs, `herd disable vm-dev`, `herd start vm-serve-mc`, `mirror.sh stop`, then `herd status` (never bare `herd`).
- Never run python from stdin on this box; write a file and run `python3 /path/x.py < /dev/null`. Do not copy keys or edit ssh/sshd config.
- Debug prints carry `[tmp]` and are listed under "Remove before final" in RESULT-pk.md. The pre-commit hook runs clippy `-D warnings` for the HOST target: run host clippy on any crate you touch before committing.

## Commit rule (you have no clock)
Commit before every build and after each piece that builds; write RESULT-pk.md early (premise rows first) and after every build, boot and commit. **Run `git push litter taoxichuan-s4` after every boot cycle that taught you something.** Never push `origin`. Commit messages say "the user", not "owner".

## Budget
About 150 tool calls and 10 boots. If a step yields no new evidence after 2 boots, say so in the plan file, re-plan from what you now know (another pass over the archive and the other kernels), and continue.

## When to stop
The gate green in one clean boot, or 10 boots with no new evidence, or a quota error. Write `/root/dev-cycle/taoxichuan-s7/RESULT-pk.md` (premise rows, gates with console evidence, syscalls added and which crate decided each, pulled/extracted/written-new, wc -l, what is next), restore ops, and end the turn.
