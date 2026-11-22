# Ecosystem demo: the first agent trial (2026-11-22)

Branch `demo-trial-run` (litter). Driver `overlays/ecosystem-demo/agents.py`, feedback round `overlays/ecosystem-demo/rework.py`,
briefs in `overlays/ecosystem-demo/briefs/`. Plan and stages 1-2: [`AKUMA_ECOSYSTEM_DEMO.md`](AKUMA_ECOSYSTEM_DEMO.md).
Run artifacts (not in the repo): `$ECO_WORK/agent-runs/<stamp>/` (transcripts, workspaces, briefs, `result.json`).

**Status: round 1 done; rework round (C fixes its board from D's report) in progress, see the last section.**

## What was run

Three Codex 0.169 agents, each in its own `box` with its own root, on the **main amd64 kernel** as both guests (QEMU+KVM, 4 GB, 2 CPUs
each), stage-2 networking (SLIRP, host alias `169.254.0.2`). **Denver was not used**: this exercises the shared userspace (`box`,
Codex, rustc/cargo, the JDK) on the main kernel; the same trial on Denver guests (`ECO_KERNEL`, `containers` feature) is the next run.

| agent | box, guest | model | task |
|---|---|---|---|
| A | the script | none | coordinator: boots, delivers briefs, runs agents, starts services, runs the oracle, collects |
| B | rust, 1 | `gpt-6.1-terra` (ChatGPT login) | Git Smart HTTP service |
| C | scala, 1 | `glm-5.3-flash` (Z.ai Coding Plan) | task board |
| D | python, 2 | `glm-5.3-flash` | acceptance tests, run against both services over HTTP |

Each agent has **its own brief** (role, tools it has, delivery rules, the contract section(s) it needs); only D sees both halves. A
delivers each through the metadata service (`/var/lib/agents/<name>/prompts/task.md`, copied into the box). The task statement
the agent receives is one line: read `/root/task.md`. Delivery rules: commit in `/work`; an executable `/work/run.sh` that starts the
service in the foreground on `$PORT` (and `$GIT_ROOT` for B); a `NOTES.md` (D: `REPORT.md`). Codex runs `exec --json` with
`sandbox_mode = "danger-full-access"` (no bubblewrap here), `code_mode_host`, and a git workspace per agent.

Order: B and C concurrently; A creates the bare repositories, starts both services from the agents' `run.sh` (`eco-box daemon`) and
checks them from guest 2; D runs with the real URLs; A runs the reference acceptance suite (`test_remote.py`) as the oracle.

Credentials: the Z.ai key and the Codex ChatGPT login are per-agent secrets (metadata service -> root-only file -> environment of
that box only), the same path as stage 1's `AGENT_TOKEN`. Z.ai serves the Responses API at `https://api.z.ai/api/v1`
(`wire_api = "responses"`); Codex 0.169 no longer speaks chat completions, and the Coding Plan token is rejected by the general
`/api/paas/v4` endpoint (error 1113). The OpenAI API key in `~/.akuma/openai` is out of credits; the Codex login is what works.

## Round 1 result

| agent | wall | Codex commands (non-zero exit) | tokens in / out | output |
|---|---|---|---|---|
| B | 734 s | 19 (4) | 0.93 M / 10 k | `src/main.rs` 87 lines driving `git upload-pack` / `receive-pack`; 1 commit |
| C | 1878 s | 114 (40) | 11.4 M / 32 k | `project.scala` 339 lines on the JDK HTTP server; 9 commits |
| D | 455 s | 20 (6) | 0.73 M / 11 k | 40 board tests + 5 git tests; 5 commits; `REPORT.md` |

Both services started from the agents' `run.sh` and answered from guest 2 (`services_up: git, board`).

* **Git service (B): passes** the reference git test and all five of D's git checks (clone, push to `main`, fetch from a second clone,
  push to a missing repository refused).
* **Task board (C): fails.** The reference suite stops at `PATCH` with an invalid value on an existing task: **404, contract says 400**.
  D's suite, written independently from the contract, reports the same defect and one the reference suite never reached:
  six `PATCH` cases (invalid `status`, `status: 1`, empty title, whitespace-only title, numeric description, boolean assignee) return
  404 instead of 400, and `POST /tasks/{nonexistent}/messages` returns 400 instead of 404 (error precedence between body and id,
  which the contract does not state). 38 passed / 7 failed of 45 checks (D's report says 46 and 41 board tests; the suite has 40 board
  tests).
* **D's suite is right**: run against the reference board on the host it passes **40 of 40** (board tests only; D's git tests were not run
  against the reference git server). The seven failures are C's, not D's.

Honest accounting: nothing was intervened on. D got no hint of the defects and found them by running its own tests, which is the
point of the tester role. D's own mistake (treating a `description`-only patch as invalid) was caught and fixed by D before the
report.

## What the trial found about the environment

| finding | detail | status |
|---|---|---|
| guest disks were too small for agents | 400 MB headroom; the first attempt died with `ENOSPC` inside Codex (`failed printing to stdout`), and C's board build had no room | **fixed**: `ECO_HEADROOM_MB` (6144 used); disks are sparse |
| Alpine moved its index | `fetch` refused: `cache differs from images/apk-lock.json` | relocked (`fetch --relock`), lock diff committed |
| `apply_patch` is not found in a box | Codex's patch tool is the binary invoked under another name; agents fall back to heredocs. Cheap for B, part of C's 114 commands | open, worth a symlink in the box root |
| scala-cli's Bloop server unusable | C's notes: "the container's Bloop backend was not usable"; it bypassed scala-cli with `scalac` + `java -cp` (`run.sh`). The stage-2 driver used `--server=false`; the brief did not say so | open: tell agents `--server=false` in the Scala brief, or fix Bloop |
| connection failures under load | C's own 100-request concurrent `curl` test saw 3 requests fail with HTTP `000` before reaching the service; C wrote them off as client-side | **unexplained**, suspect guest networking/accept backlog; reproduce with a probe |
| stage-1 capacity check | `verify`: a Scala build beside 150 JVM threads gets `EAGAIN` from `pthread_create` (peak 257 of 512 kernel threads) | open; the agent runs did not hit it |
| a non-agent port clash | the host already has a service on `:8082`; the oracle on the host used `:18082` | no action |

## Was the brief good enough?

The hypothesis to test: C's failures are a spec problem, not a model problem (`glm-5.3-flash` is strong). The first-round evidence is
mixed. Six of the seven failures are a plain defect, since the contract says invalid fields return 400 and C returned 404 for an
**existing** id (its error path appears to be a catch-all that maps to not-found), which a clearer brief would not obviously have
changed. The seventh, error precedence between a bad body and a missing id, is a real ambiguity in the contract. C also never wrote
an automated test (its NOTES say so), while the brief asked it to "test every error rule": it used an ad-hoc `curl` matrix whose
coverage was shallower than D's. The rework round below tests whether feedback closes the gap.

## Rework round (in progress)

`rework.py --run <round-1 dir>`: C's committed workspace is restored into a fresh scala box; its brief is the original plus D's
failure list and "the contract is authoritative: reproduce each, fix, re-test, commit"; C runs again; A starts the board and runs D's
committed board tests against it from guest 2. Result, token cost and diff: *to be filled in.*

## Next

* Finish the rework write-up; decide whether the contract needs the error-precedence rule (400 vs 404) spelled out.
* Rerun the whole trial on **Denver guests** (`containers` feature, `ECO_KERNEL`), same briefs, to compare.
* The `apply_patch` symlink, the Scala brief's `--server=false`, and a probe for the `curl` `000` failures.

## Background

[`AKUMA_ECOSYSTEM_DEMO.md`](AKUMA_ECOSYSTEM_DEMO.md) (plan, stages 1-2, the stage-3 summary section), [`DENVER_BOX_WIRING.md`](DENVER_BOX_WIRING.md),
[`DENVER_CODEX_TOOLS.md`](DENVER_CODEX_TOOLS.md).
