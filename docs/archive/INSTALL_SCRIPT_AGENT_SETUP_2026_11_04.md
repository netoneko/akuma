# Autonomous agents for `proposals/INSTALL_SCRIPT.md`: setup log (2026-11-04)

**Status: in progress. This records what was set up and what was observed, not results.** The Qwen endpoint is not
verified; the ryzen goose server does not exist yet.

## Goal

Work through `proposals/INSTALL_SCRIPT.md` (steps I0-I6) with autonomous agents on the two boxes. The install script
is meant to be run by a *local qwen* under an agent harness, so the first job is a Qwen the LAN can reach: it is both
the proposal's end user (step I5, the stranger test) and the model the agents' output is judged against.

## Hosts as found

| box | state at start | notes |
|---|---|---|
| trashcan (HP, `192.168.1.123`) | booted into Akuma (`hpbox.py which` -> `akuma`) | 4 cores i5-4460 (AVX2, no AVX512), 16 GB RAM (~15 GB free), 53 GB free on `/`, no GPU. `/bin/llama-server`, `curl`, `git`, `gcc` present; no `goose`, no `.gguf` over 10 MB. `huggingface.co` answers HTTPS from the box. A herd conf already runs `llama-server` with `/models/smollm2-135m.gguf` on `127.0.0.1:8081`. |
| ryzen (`192.168.1.159`) | first probe: `ssh ... port 2222: Operation timed out` (it was on Pop, not Akuma); later `up 2 min` on Pop | rebooted to Akuma, see below |

## Decisions

- **Model: Qwen3-8B Q4_K_M (~5 GB), fallback Qwen3-4B.** CPU-only Haswell, 4 threads: an 8B at Q4 fits with room for
  context and should do single-digit tok/s; the 30B-A3B MoE at Q4 (~18 GB) does not fit in 16 GB. Expect slow; the
  measured number goes in the agent's RESULT file.
- **The GLM goose server runs on the Mac, not the box**, because ryzen was unreachable when the task started and
  because trashcan has no goose. Script: `scripts/kvm/goose/goose-glm-mac.sh serve --port 3286` (`glm-5.3-flash`,
  low effort, z.ai token from `~/.akuma/z.ai/token`, ACP key `~/.akuma/goose/acp_secret_key`). Probe (`pong`) passed.
  The agent reaches trashcan through `scripts/utils/hpbox.py ak '<cmd>'`.
- Brief delivered as a **file** with a three-line prompt (the runbook's rule: an 8 KB prompt stalled two sessions).
  The brief lives in the session scratchpad, not the repo; its content is summarised under "Brief" below.

## What was run

1. Read `proposals/INSTALL_SCRIPT.md`, `scripts/kvm/goose/SUPERVISOR_RUNBOOK.md`, `BRIEF_TEMPLATE.md`.
2. Surveyed trashcan over `hpbox.py ak` (numbers above).
3. Started the Mac goose-glm server on `127.0.0.1:3286` and probed it. The first probe failed with
   `KeyError: 'result'`: `session/new` needs a **Mac** `cwd` (`GOOSE_ACP_CWD`), the same trap the runbook records for
   the Kimi server. With it set the probe answered `pong`.
4. Sent the brief as session `qwen-trashcan` (`scripts/goose_acp_prompt.py`, 4 h budget). Log:
   `glm-qwen.log` in the session scratchpad.
5. Rebooted ryzen from Pop to Akuma: `hpbox.py rzr 'cd .../akuma && sh overlays/ryzen/arm.sh 29'` (GRUB entry 29;
   `next_entry=29` confirmed, reboot issued 17:18). **Not yet confirmed that it came up**; use `scripts/vm_ready.py`
   style ssh round-trip (`scripts/kvm/goose/ak.py 'uname -a'`), never a log grep.

## Brief (summary)

Download Qwen3-8B Q4_K_M to `/models/` (background `curl -C -`, size check against Content-Length); prove load and
generation on `127.0.0.1:8082` first and record load time and tok/s; create a new herd conf serving
`--host 0.0.0.0 --port 8081`, stopping and disabling only the smollm2 service that holds that port; verify from the
Mac with `/v1/models` and a chat completion, then `herd restart` and verify again; write
`scripts/kvm/goose/RESULT-qwen-trashcan.md` (sha256, conf, tok/s, memory, deviations). Forbidden: reboot, `pkill`,
git writes, sub-agents, touching console/sshd/kot/httpd/hda-capture services.

## Observed so far

- After ~45 s the agent had made 7 tool calls and was surveying the box. Its first finding: `which`/`df /` output
  looked wrong (`df /` showed only proc) and it went looking for `curl` and the real root fs. Unresolved at the time
  of writing; the box's own `df` earlier showed `/dev/sda1` 64 GB, so this may be an Akuma `df`/PATH quirk through the
  non-login ssh shell. Check the agent's log before trusting its conclusion.

## Open

- Qwen endpoint: unverified. Nothing was confirmed reachable from the LAN.
- Ryzen: needs the Akuma boot confirmed, then a `goose serve` in the Taoxichuan environment (herd service
  `goose-glm`, `192.168.1.159:3284`, per `SUPERVISOR_RUNBOOK.md` § Setup) and a first brief for the remaining
  `INSTALL_SCRIPT.md` steps. Not started; the user wants it begun when the Qwen is downloaded. Candidate split: I0
  (fresh-clone proof) and I1 (x86_64 `install.sh`) on ryzen where KVM and the Taoxichuan kit already work; I4
  (arm64 minimal disk) cannot be done there.
- Whether the Mac goose server should stay once ryzen's is up.

## Background

`proposals/INSTALL_SCRIPT.md`, `scripts/kvm/goose/SUPERVISOR_RUNBOOK.md`, `scripts/kvm/goose/goose-glm-mac.sh`,
`scripts/utils/hpbox.py`, `overlays/ryzen/arm.sh`.
