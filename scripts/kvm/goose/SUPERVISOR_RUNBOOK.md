# Supervising goose-glm through the Taoxichuan ladder - the loop that worked (2026-10-26)

Read first: `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md` (§ Process, § Session protocol), the TOP of
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` plus its last sections (stage 7 D-G, methodology pivot, supervisor lapse), and
`scripts/kvm/goose/BRIEF_TEMPLATE.md`. Repo rules: never `git reset`/rebase/rewrite history; the owner has authorised commit + push to `origin rust-org-il-experiment`
for this work (never `litter`, goose pushes that), and force-removal of worktrees. Never launch sub-agents. Never use `python3 -` / heredoc python on the box.

## Setup
- Box (ryzen, Akuma): `ssh -p 2222 root@192.168.1.159` through `scripts/kvm/goose/ak.sh '<cmd>'` (the `ssh` CLI may be blocked; run it from a script). If the box is on Pop:
  `python3 -I scripts/utils/hpbox.py rzr 'cd /home/netoneko/github.com/netoneko/akuma && sh overlays/ryzen/arm.sh 29'` boots Akuma (GRUB entry 30). The box has no tty: no `-o` flags on its own ssh.
- On the box, helper scripts live in `/tmp` and vanish on reboot: re-create `/tmp/gss.py` from `scripts/goose_session_stats.py` and `/tmp/watch.py` from `scripts/kvm/goose/watch_session.py`
  (`ak.sh 'cat > /tmp/x.py' < file`; run with `python3 /tmp/x.py ARGS < /dev/null`).
- goose: `goose serve` on `192.168.1.159:3284` (herd service `goose-glm`, model `glm-5.3-flash`, low effort), driven from the Mac by `scripts/goose_acp_probe.py` (pong test) and
  `scripts/goose_acp_prompt.py BRIEF --out LOG --name X` (log has the `session:` id). Mid-turn corrections: `scripts/goose_acp_steer.py SESSION_ID TEXT_FILE`.

## One milestone = one session
1. Write the brief from `BRIEF_TEMPLATE.md` (key fact, read-first docs/probes, composition rule 1-4 incl. 3b, gates with exact commands, cost rules, ops traps, event-based commit rule,
   stop rule). Diff its sections against the template; verify every address/command in a gate against a doc or a passing run. NO hints about the bug, docs and probes yes.
2. **Deliver the brief as a file, not as the prompt** (an 8 KB prompt stalled two sessions): `ak.sh 'cat > /root/dev-cycle/taoxichuan-s7/BRIEF-X.md' < brief.md`, then send a 3-line prompt:
   "Your task brief is the file ... Read it in 60-line windows and carry it out. Start every shell command with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`."
3. **One clean goose server before every session**: `herd stop goose-glm` kills only the wrapper; also `kill -9` every pid whose `ps` line has `goose serve`, then `herd start goose-glm`, wait ~10 s,
   run the probe (`pong`), THEN send. Never send while a driver (`goose_acp_prompt.py`) is alive. Never run goose/crush wrappers on the box (they overwrite the shared secrets).
4. Poll inside the turn (a turn that ends cannot wake itself): sleep ~280 s, then `grep -c tool_call LOG`, `git log --oneline -3` and `ls /root/dev-cycle/taoxichuan-s7 | grep RESULT` on the box,
   goose's last narration (`grep "agent:" LOG | tail -50 | sed 's/.*agent: //' | tr '\n' ' '`), or `watch_session.py SESSION_ID` on the box for the db view. Report to the owner every ~5 min.
   A silent stretch > 7 min with no running process = the model call hung: restart (step 3) and resend; do not wait it out.
5. When it ends (driver gone, `RESULT-*.md` present): read the result, `git fetch litter taoxichuan-s4`, merge into `rust-org-il-experiment`, `scripts/build_taoxichuan_fc.sh`,
   `FC_KERNEL=taoxichuan-fc.bin overlays/devbox-firecracker/run.sh --no-net` (Lima `fc`; console in `/tmp/akuma-fc-boot.log` inside Lima: `limactl shell fc -- cat`), expect ~162 lines, `Ext2 filesystem mounted`,
   `sshd started`, 0 `Fault|PANIC|sig!`. Add the host-track + metrics entry to the agent log, commit, push `origin`. Pre-commit runs clippy/tests: fix, never `--no-verify`.
6. Metrics per session (`ak.sh "python3 /tmp/gss.py 'Taoxichuan' < /dev/null"`): calls, tokens, docs reads; plus pulled/extracted/cut/written-new from the RESULT file, `wc -l taoxichuan/src/*.rs`,
   `akuma-*` dependency count of `taoxichuan/Cargo.toml` (18 at the end of stage 7).

## Box traps (each cost real time)
- Install the guest kernel only with an explicit destination `/boot/akuma-guest-dev`; never write `/boot/akuma-amd64*`; the host kernel is not rebuilt.
- herd re-parses a conf only on `disable` then `enable`; a reload can start a second guest; truth is `/var/run/herd/vm-dev.vm`. kvm0 is exclusive (`vm-serve-mc` halted while `vm-dev` runs); restore at the end.
- The ssh client capture loses output and the forward degrades near 30 connections per boot: judge by the console (`[SSH] Exit status`), `run_gates.sh` does this.
- `grep` on the Mac is ugrep (binary detection): use `-a` on logs. `git push litter` on the box needs `. /etc/akuma-dev.env`.
- The owner reports quota percentages (5 h window, weekly); a fresh-context session costs ~1-3M input tokens, a long one 20M+. z.ai weekly resets are manual.
