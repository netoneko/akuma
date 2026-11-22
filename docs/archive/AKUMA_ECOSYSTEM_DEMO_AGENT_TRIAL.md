# Ecosystem demo: the first agent trial (2026-11-22)

Branch `demo-trial-run` (litter). Driver `overlays/ecosystem-demo/agents.py`, feedback round `overlays/ecosystem-demo/rework.py`,
briefs in `overlays/ecosystem-demo/briefs/`. Plan and stages 1-2: [`AKUMA_ECOSYSTEM_DEMO.md`](AKUMA_ECOSYSTEM_DEMO.md).
Run artifacts (not in the repo): `$ECO_WORK/agent-runs/<stamp>/` (transcripts, workspaces, briefs, `result.json`).

**Status: round 1 and the rework round are done. Round 1: git passes, board fails. After one feedback round the board passes both D's suite and the reference suite.**

## What was run

Three Codex 0.169 agents, each in its own `box` with its own root, on the **main amd64 kernel** as both guests (QEMU+KVM, 4 GB, 2 CPUs
each), stage-2 networking (SLIRP, host alias `169.254.0.2`). **Denver was not used in round 1**: it exercises the shared userspace (`box`,
Codex, rustc/cargo, the JDK) on the main kernel; round 2 below repeats it on Denver guests.

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

## Rework round (done)

`rework.py --run <round-1 dir>`: C's committed workspace is restored into a fresh scala box; its brief is the original plus D's
failure list (the "Failed" section of `REPORT.md`) and "the contract is authoritative: reproduce each, fix, re-test, commit"; C runs
again; A starts the board and runs D's committed board tests from guest 2. Then `judge.py` re-judges the saved work of B and C with
the reference suite alone (no agents, no credentials).

| | round 1 (C) | rework (C) |
|---|---|---|
| wall | 1878 s | **224 s** |
| Codex commands (non-zero exit) | 114 (40) | 21 (9) |
| tokens in / out | 11.4 M / 32 k | 0.48 M / 3.9 k |
| D's 40 board tests | 33 pass, 7 fail | **40 pass** |
| reference suite (board + git, B's git service) | board fails | **OK, 2 of 2** (`judge.py`, 51 s) |

The fix was two commits' worth, 6 added and 2 removed lines in `project.scala`: `PATCH` now validates the fields *before* the
lookup (`case None => errors.invalid`), and a message `POST` to a nonexistent id returns 404 before its body is read. So the
seven failures were one cause, the order of checks: the contract's validation was reachable only after a not-found. Feedback
closed it in under four minutes and one twentieth of the tokens, because the report named the cases and the contract was
unambiguous about all but the precedence case.

**What this says about the brief.** The first-round defect was not a missing requirement (the contract states it) but an
unverified one: C tested with an ad-hoc `curl` matrix and never wrote the invalid-field-on-existing-id case. The brief asked it to
cover "every error rule"; GLM judged that done. The contract still has one gap worth closing: **precedence between a bad body and a
missing id** (the rework chose 404 first for messages and 400 first for `PATCH`; the reference suite accepts both). Spell it out
in `spec/SPEC.md` or accept either.

Environment notes from the rework: none new. (`rework.py`, `agents.py` and `judge.py` originally ended their in-box script with
`exit $rc`, which skips `eco-box`'s status file and made the reported exit status 255 while the output said `OK`; fixed to
`(exit $rc)`. Round 1's oracle line `rc=0` came from `tail`, fixed earlier.)

## Round 2: the same trial on Denver guests (2026-11-22)

Same briefs, scripts and models; only the guest kernel changed (`ECO_KERNEL=target/x86_64-unknown-none/release/denver`, built with
`--features containers,codex`). GLM quota before: 5 h window 32 %, week 6 % (`scripts/kvm/goose/zai_usage.py`); after: 36 % / 7 %.

| agent | main kernel (round 1) | Denver (round 2) |
|---|---|---|
| B rust (`gpt-6.1-terra`) | 734 s, 19 commands | **327 s**, 14 commands; git service built and **UP**, answers from guest 2 |
| C scala (`glm-5.3-flash`) | 1878 s, 9 commits, board up (7 defects) | **hung, timed out at 2400 s (rc=124)**; its commit has 3 files but the board never started |
| D python (`glm-5.3-flash`) | 455 s, 46 tests | 213 s; 5 git checks pass, 39 board tests fail with `Empty reply from server` (nothing listening); D correctly called it a deployment defect |

Rust and `cargo build` on Denver therefore work, and faster than on the main kernel in this run (the hosts were not otherwise idle, so
treat the ratio as indicative, not measured). The Scala board did not come up, so the reference oracle was not run.

**The hang, as observed in the live guest** (Codex alive and polling `write_stdin` every 30 s, 323 session events, one open
connection to z.ai; no out-of-memory: 2.9 of 4 GB free, 5.4 GB disk free, nothing in the serial log): the chain was
`codex` -> `scala-cli.jar run --server=false . --main-class TaskBoard` (JVM) -> the Scala 3 compiler JVM
(`dotty.tools.dotc.Main`). Load average 0.00, the compiler's CPU time frozen at about 3 s (mostly JIT threads) for 30+ minutes,
no sockets open in the guest besides Codex's, and the compiler's stdout is `/dev/pts/0` (Codex's pty). `SIGQUIT` to the compiler
made the serial log print `socket(domain=10): unsupported` / `bind(port=0)` pairs (IPv6 refused, retried over IPv4: the JVM's
signal path is alive). Codex kept spending about 25 k input tokens per poll while waiting.

**Not reproduced by hand, cause unknown.** `scalahang.py` boots guest 1 only and starts a Scala service with `scala-cli run`:
the rework round's JDK-only board answers `/health` in 37 s on Denver today **and** on Denver built before `be5b2d86`
(socket wait hooks), so today's merges (C3's slot ring, the shared wait hooks) are not implicated for that workload. C's actual
project (`//> using dep com.lihaoyi::ujson:3.3.0`) did not hang in the repro: it **failed to compile** (7 errors, `geny` not
found) on Denver, pre-merge Denver and the main kernel alike, i.e. a dependency-resolution problem in that repro, not a kernel
difference, so that comparison proves nothing about the hang. What differs in the real run and is not yet tried: Codex's pty as
stdout, the process tree under a Codex exec session, and the ujson artifacts resolved in the agent's session. Next: run the same
project with a pty (`ssh -t`) and with the ujson jars pre-cached, and under Codex without an agent (`codex exec` with the fake
model, `boxed-codex-probe.py`).

Environment notes added by this round: `apply_patch: not found` costs C its first command again (open, see above); `ps -o pcpu`
is unsupported in the busybox `ps` (agents work around it); `ps` shows `etime` frozen (identical for every process) on Denver.

### Round 2 follow-up: `scala-cli run` on Denver, narrowed (2026-11-22)

`scalarun.py` (plain or under a Codex exec session), `scalahang.py`, `reaperprobe.py` and `execthread-probe.py` against the Denver
kernel from this branch (`containers,codex`) and the main amd64 kernel, same images, no agent unless stated.

| test | main kernel | Denver |
|---|---|---|
| `scala-cli run` of a **server** (the rework board, `/health`) | n/a | answers in 37 s |
| plain, a program that **exits** (JDK-only hello; `ujson` hello) | not run | **prints its output (`PLAIN_OK 55`) and the command never completes** |
| plain, C's project (7 compile errors) | n/a | finishes in 33 s, errors printed |
| under a Codex exec session (pty), C's project | **finishes, 64 s** | **hangs** |
| under a Codex exec session, `ujson` hello | not run | **hangs** |
| `wait4` from a thread other than the forking one (4 layouts, Python) | all reaped | all reaped |
| threaded `execve` probe (host and box, success and failure, 3 each) | 0 failures | 0 failures |

* **Not a regression from this session's merges.** `git bisect` over the 7 kernel-touching commits since `e00744db` (the commit the docs
  say ran `scala-cli run` on Denver) found every one hanging on the `ujson` hello, **including `e00744db` itself**. The docs'
  passing runs all ran *servers*, which never have to exit; no earlier test ran a program that terminates.
* **The work completes; the process does not.** In the hello tree the app JVM has printed and finished (a zombie, `Z`, child of the
  `scala-cli` JVM) and the parent JVM, left with one thread, never reaps it. Servers, plain compile errors and Rust (`cargo
  build`, B's whole run) are fine. That also explains C's trial: it was not compiling, it was waiting on a build that had already
  failed (under Codex's pty, where even the error exit hangs).
* **Not the mechanisms tested here.** Cross-thread `wait4` and the static threaded-exec program both pass on Denver, so the stall
  needs something the JVM does that those do not (a `jspawnhelper`/`vfork` spawn, signals, the process-reaper thread count, the
  pty hangup). Open: a reproducer without a JVM, or a `strace_err`/serial trace of the final `wait4`/`exit_group`, on Denver.
* **Consequence for the demo:** a Denver Scala agent can build and run a *server* (the reference board does), but an agent's own
  test loop (`scala-cli run` of anything that exits, a compile error under Codex) stalls it. Until fixed, rerun C with the brief
  telling it to run the board only in the background with output to a file, or run C's box on the main kernel.

## Next

* Decide whether the contract spells out the error-precedence rule (400 vs 404).
* Find why a terminating `scala-cli run` never completes on Denver (follow-up above), then rerun C on Denver.
* The `apply_patch` symlink, the Scala brief's `--server=false`, and a probe for the `curl` `000` failures.

## Background

[`AKUMA_ECOSYSTEM_DEMO.md`](AKUMA_ECOSYSTEM_DEMO.md) (plan, stages 1-2, the stage-3 summary section), [`DENVER_BOX_WIRING.md`](DENVER_BOX_WIRING.md),
[`DENVER_CODEX_TOOLS.md`](DENVER_CODEX_TOOLS.md).
