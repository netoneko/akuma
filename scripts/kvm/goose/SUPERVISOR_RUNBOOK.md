# Supervising goose-glm through the Taoxichuan ladder - the loop that worked (2026-10-26)

Read first: `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md` (§ Process, § Session protocol), the TOP of
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` plus its last sections (stage 7 D-G, methodology pivot, supervisor lapse), and
`scripts/kvm/goose/BRIEF_TEMPLATE.md`. Repo rules: never `git reset`/rebase/rewrite history; the user has authorised commit + push to `origin rust-org-il-experiment`
for this work (never `litter`, goose pushes that), and force-removal of worktrees. Never launch sub-agents. Never use `python3 -` / heredoc python on the box.

## Setup
- Box (ryzen, Akuma): `ssh -p 2222 root@192.168.1.159` through `python3 scripts/kvm/goose/ak.py '<cmd>'` (Python; no kill timeout; `--stdin` sends stdin; if the box moved on DHCP it is found by its fixed MAC through `overlays/ryzen/wifi-ssh.py`). `ak.sh` is the older shell form that `retry_goose2.sh` still calls. If the box is on Pop:
  `python3 -I scripts/utils/hpbox.py rzr 'cd /home/netoneko/github.com/netoneko/akuma && sh overlays/ryzen/arm.sh 29'` boots Akuma (GRUB entry 30). The box has no tty: no `-o` flags on its own ssh.
- On the box, helper scripts live in `/tmp` and vanish on reboot: re-create `/tmp/gss.py` from `scripts/goose_session_stats.py` and `/tmp/watch.py` from `scripts/kvm/goose/watch_session.py`
  (`ak.sh 'cat > /tmp/x.py' < file`; run with `python3 /tmp/x.py ARGS < /dev/null`).
- goose: `goose serve` on `192.168.1.159:3284` (herd service `goose-glm`, model `glm-5.3-flash`, low effort), driven from the Mac by `scripts/goose_acp_probe.py` (pong test) and
  `scripts/goose_acp_prompt.py BRIEF --out LOG --name X` (log has the `session:` id). Mid-turn corrections: `python3 scripts/kvm/goose/steer.py SESSION_ID TEXT_FILE` - it sends the steer with a marker, exits nonzero with `NOT DELIVERED` on any failure, requires the message to be in goose's session database, and reports `consumed=yes|no` (the agent reads steers at its next turn; ask again with `--check-only TAG`). Never wrap it in a short `timeout`: a silent kill was once read as "sent" and two steers never arrived.
  Watchdog: `retry_goose2.sh` (not `retry_goose.sh`) - model silence 540 s, but 1800 s while the last log event is an unfinished tool call, plus `alive:` heartbeat lines while the model streams thinking, and a 12 h turn budget.

## One milestone = one session
1. Write the brief from `BRIEF_TEMPLATE.md` (key fact, read-first docs/probes, composition rule 1-4 incl. 3b, gates with exact commands, cost rules, ops traps, event-based commit rule,
   stop rule). Diff its sections against the template; verify every address/command in a gate against a doc or a passing run. NO hints about the bug, docs and probes yes.
2. **Deliver the brief as a file, not as the prompt** (an 8 KB prompt stalled two sessions): `ak.sh 'cat > /root/dev-cycle/taoxichuan-s7/BRIEF-X.md' < brief.md`, then send a 3-line prompt:
   "Your task brief is the file ... Read it in 60-line windows and carry it out. Start every shell command with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`."
3. **One clean goose server before every session**: `herd stop goose-glm` kills only the wrapper; also `kill -9` every pid whose `ps` line has `goose serve`, then `herd start goose-glm`, wait ~10 s,
   run the probe (`pong`), THEN send. Never send while a driver (`goose_acp_prompt.py`) is alive. Never run goose/crush wrappers on the box (they overwrite the shared secrets).
4. Poll inside the turn (a turn that ends cannot wake itself): sleep ~280 s, then `grep -c tool_call LOG`, `git log --oneline -3` and `ls /root/dev-cycle/taoxichuan-s7 | grep RESULT` on the box,
   goose's last narration (`grep "agent:" LOG | tail -50 | sed 's/.*agent: //' | tr '\n' ' '`), or `watch_session.py SESSION_ID` on the box for the db view. Report to the user every ~5 min.
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
- The user reports quota percentages (5 h window, weekly); a fresh-context session costs ~1-3M input tokens, a long one 20M+. z.ai weekly resets are manual.

## Kimi as a planning second opinion on the Mac (the "middle robot", 2026-10-28)
Kimi (goose, `k3-256k`, low effort) plans on the Mac and the supervisor carries its document to goose-glm. Kimi never touches the box: `goose-kimi` and `goose-glm` write the same `secrets.yaml` there, and goose with Kimi has failed on amd64 before (`docs/archive/AKUMA_AMD64_AGENT_STAGING_AND_ACCOUNTING.md` § 2).
- **Server:** `scripts/kvm/goose/goose-kimi-mac.sh serve --port 3285` (its own config dir `~/.config-goose-kimi`, key read from crush's config, ACP key in `~/.akuma/goose/acp_secret_key`). A server from 2026-10-24 may already hold 3285 (`lsof -nP -iTCP:3285`): reuse it, a second one fails with "Address already in use".
- **Workspace:** a scratch worktree, never the main checkout: `git worktree add --detach $SCRATCH/kimi-wt litter/taoxichuan-s4`, then copy in any newer docs (the agent log, the latest RESULT/PLAN archives). Remove it afterwards (`git worktree remove --force`).
- **Send:** `GOOSE_ACP=http://127.0.0.1:3285/acp GOOSE_ACP_KEY_FILE=$HOME/.akuma/goose/acp_secret_key python3 -I scripts/goose_acp_prompt.py PROMPT --out LOG --name kimi --cwd $SCRATCH/kimi-wt --budget 43200` with a 3-line prompt that points at the brief file in the worktree (`kimi-plan-preemption.md` is the first one). The probe script fails with "invalid directory path" unless it gets a Mac `--cwd`; that is not a server fault.
- **Quota:** `GET https://api.kimi.com/coding/v1/usages` with the token in `~/.akuma/kimi/token` (print only the numbers). On 2026-10-28 the 5 h window was 100/100 remaining and the month 69.8% used, resetting 2026-10-30 00:00Z. A request is a tool round trip: briefs tell Kimi to batch tool calls.
- **Relaying:** put Kimi's document on the box next to the other plans (`ak.py --stdin 'cat > /root/dev-cycle/taoxichuan-s7/KIMI-PLAN.md' < PLAN-kimi.md`), check it against the docs for plain errors first, and tell goose-glm only that it exists, what it is, and to compare it with its own plan and decide for itself. Do not paraphrase Kimi's design into steers: the comparison is only worth anything while each plan is attributable to one model.
- **No mid-turn steer on the Mac goose (observed 2026-10-28, goose 1.41.0):** `goose_acp_steer.py` reaches the secured server (stream 200, `session/load` 202) but never gets an answer to `session/load` while a turn is running, so it reports `NOT DELIVERED: no answer to session/load`. The ryzen server answers the same call mid-turn. Until that is explained, a correction to Kimi goes in as a follow-up prompt into the same session after its turn ends: `GOOSE_ACP=http://127.0.0.1:3285/acp GOOSE_ACP_KEY_FILE=$HOME/.akuma/goose/acp_secret_key python3 -I scripts/goose_acp_say.py SESSION_ID TEXT_FILE $SCRATCH/kimi-wt`. Pass the Mac worktree path as the cwd for every call to this server: the box's `/root` default makes `session/new` fail with "invalid directory path".
