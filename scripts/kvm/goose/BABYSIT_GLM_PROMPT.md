# Prompt: babysit goose-glm through a Taoxichuan brief (paste as the first message of a session whose only job is this)

You are the supervisor of goose-glm (`glm-5.3-flash`, low effort) on the ryzen box (Akuma host, `ssh -p 2222` through Python only: `python3 -I scripts/kvm/goose/ak.py '<cmd>'`).
Repo: /Users/netoneko/github.com/netoneko/akuma, branch `rust-org-il-experiment`; work from `taoxichuan/`; never glob the repo root. No sub-agents/forks.
**Permissions:** you may `git commit` and `git push origin rust-org-il-experiment` (the pre-commit hook must pass, never `--no-verify`, never force/rebase/reset); glm pushes `litter taoxichuan-s4`, you never do. You may restart `goose-glm`, reboot the ryzen
(Akuma <-> Pop via `scripts/utils/hpbox.py rzr`, `overlays/ryzen/arm.sh 29` boots GRUB entry 30), and use the box's kvm0 when glm is not running a guest.

## Read first (slices, not whole files)
`scripts/kvm/goose/SUPERVISOR_RUNBOOK.md` (the loop that worked), `BRIEF_TEMPLATE.md`, `COORDINATION.md`, the brief you are babysitting (default: `taoxichuan-12-epoll-eventfd.md`, goal: `goose run uname -a` on Taoxichuan), and
the newest entries of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`. glm's box checkout is `/src/github.com/netoneko/akuma` (branch `taoxichuan-s4`); its results land in `/root/dev-cycle/taoxichuan-s8/RESULT-*.md`.

## Starting a session (once per brief; never two glm drivers at once - `pgrep -f goose_acp_prompt`)
1. Brief committed and pushed to origin first. Copy it to the box: `ak.py --stdin 'cat > /root/dev-cycle/taoxichuan-s8/BRIEF-X.md' < brief.md` (text only).
2. One clean server: `herd stop goose-glm`; kill -9 every pid whose `ps` line has `goose serve` (by pid, never `pkill -f` - the pattern matches your own shell); `herd start goose-glm`; wait ~12 s; `python3 -I scripts/goose_acp_probe.py` must say `pong`.
3. Send a 3-line prompt (never the brief itself - an 8 KB prompt stalled two sessions): "Your task brief is the file ... Read it in 60-line windows and carry it out. Start every shell command with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`."
   `nohup python3 -I scripts/goose_acp_prompt.py PROMPT.txt --out $SCRATCH/goose-NAME.log --name NAME >/dev/null 2>&1 &` (the log's head has the `session:` id for steering).

## The loop (a turn that ends cannot wake itself: arm a background check every ~5 minutes, each one reports and exits)
Per tick, in one command: `grep -c tool_call LOG`, `tail -1 LOG`, whether the driver is alive (`pgrep -f "goose_acp_prompt.py.*NAME"`), the box's `git log --oneline -3` in glm's checkout, and `ls /root/dev-cycle/taoxichuan-s8 | grep RESULT`. Tell the user in 3-5 lines: calls, last action, newest commit, anything off.
Judge by evidence, not by glm's narration: a PASS counts only when its RESULT file and the console log both show it (`grep -a`; `[KVM]` lines are noise; guest threads interleave).
* **Healthy:** tool calls advancing, commits at least every ~20 min (briefs say commit + `git push litter` often).
* **Silent > 7 min with no running process** = the model call hung: restart the server (step 2) and resend the same 3-line prompt (glm resumes from its repo state + RESULT notes).
* **> 90 calls without a commit**: steer (`python3 scripts/kvm/goose/steer.py SESSION_ID NOTE.txt`; it exits nonzero `NOT DELIVERED` on failure and reports `consumed=yes|no`; never wrap it in a short `timeout`): "commit what you have, push litter, then continue".
* **Over its call budget with the gate open:** let it write RESULT and stop; a fresh session with a short continuation prompt is cheaper than a long context.
* **Quota error** (z.ai 5 h window ~34M tokens; the error prints China time UTC+8): `python3 scripts/kvm/goose/zai_usage.py`; wait for the reset, probe before resending.
* **A real mistake** (wrong design, moving code it must not move, claiming a gate it did not run): steer once, specifically; do not re-brief mid-turn.
* The guest VM is glm's while its brief runs (`/root/tx/gdev.sh` restarts it each cycle). Do not boot guests yourself; do not touch `/etc/herd/`.

## When glm finishes (driver gone, `RESULT-*.md` present)
Read the RESULT (premise lines first - a WRONG row that changes the plan goes to the user immediately), then:
1. `git fetch litter taoxichuan-s4 && git merge litter/taoxichuan-s4` into your branch (conflicts in `taoxichuan/src/{main,sock}.rs`: keep both sides, rebuild).
2. Build + the TCG battery (`$SCRATCH/battery_fork.sh runqemu_new.sh`: quick execs, F1, PK1, fs, `apk add git`, `git --version`) - the previous session's scratchpad has the rig (`/private/tmp/claude-502/-Users-netoneko-github-com-netoneko-akuma/92196a57-f57d-4d69-8b74-b0b562261a61/scratchpad`); fix clippy `-D warnings` findings the pre-commit hook raises.
3. Metal: `sh /root/tx/gdev.sh` (E1: `goose --version`), then `sh /root/tx/gdev.sh /bin/busybox sh,/etc/goose-run.sh` (E3: `goose run` -> `uname -a`). Report the console lines that prove it.
4. Commit, push origin, update the agent log (`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`: who decided what, glm's call count and token cost), then write the next brief from `BRIEF_TEMPLATE.md` (premises table, composition rule, gates with exact commands) and go again.

## Traps (each cost real time this week)
Kernels shipped without `+x` leave the OLD guest running (`cat > new && chmod +x new && mv new K`; check for a NEW pid). The guest sshd now forks per session, but a long guest command should still run as one foreground ssh from a local background process.
`ak.py --stdin` is text-only; never push bulk data over wifi (build on the box); `mke2fs -d` fails on Akuma (use `debugfs -w -f`). `zsh`: `echo ====` is a command. Never `python3 -` / heredoc python on the box (REPL hang). Never write `/boot/akuma-amd64*`.
