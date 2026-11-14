# Denver autoresearch runbook — how the agent iterates

**Who reads this:** the agent (goose + an open-weight model), at the start of every session, before
anything else. The plan it serves is
[`docs/archive/KERNEL_HOWTO_BUILDING_CUSTOM_PLATFORM_KERNEL.md`](../archive/KERNEL_HOWTO_BUILDING_CUSTOM_PLATFORM_KERNEL.md)
(frozen — it is what was designed); this runbook is how it actually gets done, and it is the only file
allowed to change the *procedure* (never silently — every change is a decision entry in
`LOOP_LOG.md`).

One session = one gate. Start the loop, finish with a record, leave one line saying what the next
gate is. Never run two gates at once; a session that touches two capabilities is a session that
proves nothing.

## Where everything lives

| path | what it is | who writes it |
|---|---|---|
| `denver/RESULT.md` | the experiment record: every gate with command + captured output, every intervention, wall clock and token spend | the agent |
| `denver/docs/REQUIREMENTS.md` | the gap table from lesson 1: gap → evidence → closing pass; re-ranked only with a recorded reason | the agent |
| `denver/docs/lessons/L*.md` | the **living** lesson runbooks (Part I) and the pass template (Part II), in the lesson style | the agent, after each gate |
| `denver/docs/LOOP_LOG.md` | Part II: one entry per pass, Record voice — what we did, what decision it forced | the agent |
| `denver/docs/research/<pass>.md` | the research-first artifact, one per pass (rule R1) | the agent |
| `denver/ESCALATE.md` | the escape hatch: `QUESTION:` / `CONTEXT:`, answered by a human or redone by a stronger model; archived to `denver/docs/archive/escalations/` once answered | the agent asks, the human or the bigger model answers |
| `denver/docs/archive/AUTORESEARCH_WORK_LOG.md` | the log of the autoresearch effort itself: issues in the loop, script fixes, protocol changes, escalations | everyone who touches the machinery |
| `denver/docs/archive/` | the plan as designed; history, not current state | nobody |
| `denver/gates/<gate>.log` | captured probe output — the only thing that judges a gate | the agent |
| `../..//scripts/kvm/goose/denver-01-autoresearch.md` | the goose recipe/brief that starts a session | maintained by hand |
| `denver/autoresearch.sh` | the session driver: picks the next gate from `NEXT GATE:`, fires goose, judges bookkeeping only (`sh denver/autoresearch.sh [sessions] [gate]`) | the human or a scheduler |

The split that matters: **the archive plan is frozen, the lessons are living.** After each gate the
agent rewrites the lesson's *Do* steps from expected → as-happened, quotes every place the plan was
wrong (the shanghai-01 rule), and appends the *Record*. Adjustments are open, logged, and never a
silent edit of the past.

## The session loop

1. **Orient.** Read this runbook, the tail of `LOOP_LOG.md`, `RESULT.md`, `git log --oneline -5`,
   `git status`. Name to yourself: phase, current gate, what the last decision was.
2. **Pick the one next gate** from `REQUIREMENTS.md` / the phase definition. If the previous session
   left a next-gate line, take it unless a recorded reason says otherwise.
3. **Research first (R1).** No code before `denver/docs/research/<pass>.md` exists. See the rules
   below for what it must contain.
4. **Implement the smallest change** behind the planned feature. Throwaway patches in QEMU are the
   loop working; WIP commits (`--no-verify`, must build, every ~15 minutes — see R8) protect the
   work; the gate's record still commits only on evidence, and the driver does that.
5. **Gate.** Boot under Akuma KVM (or QEMU/KVM), run the probe, capture output to
   `denver/gates/<gate>.log`. A gate passes when the named probe line appears in that captured
   output — never on the agent's belief that it should.
6. **On failure:** one hypothesis → one change → re-run. After **three** hypotheses without a verdict,
   invoke the stall protocol (below). Fix diagnostics before bugs: a pass that makes the failure
   *readable* outranks a pass that guesses at the fix.
7. **Record.** `LOOP_LOG.md` entry (what we did, what decision it forced), lesson *Do*/*Record*
   update, `RESULT.md` row — then the driver script commits and pushes: docs and records with
   `--no-verify`, code only after targeted `cargo test -p` on the crates the session touched.
   The agent's job ends at the record.
8. **Stop.** Last line of `LOOP_LOG.md`: `NEXT GATE: <name>` — the next session's step 2.

House rules that hold every session: branch `denver`, push to `litter` only, never `origin`; never
kill a VM by pattern, stop only the one you started (its pid file); the driver script does the
committing — and in batch mode a z.ai quota death is a *wait*, not a failure: the driver reads the
log tail, sleeps `QUOTA_WAIT_SECS`, and retries the same session with the budget untouched.

## Phases, and when to move

| phase | contains | done when |
|---|---|---|
| **0 — scaffold** | `denver/` + `crates/akuma-jvm-kernel/` crates exist; lesson 3's gate | Denver boots tokio, `uname` says denver |
| **1 — method** | lessons 1, 2, 4: requirements table, RED probes, measured loop | `REQUIREMENTS.md` complete; every probe fails cleanly; one hypothesis-to-verdict cycle measured under an hour |
| **2 — passes** | Part II, one pass per capability, ranked by `REQUIREMENTS.md` | no open gap that blocks phase 3; each pass has research + gate + record |
| **3 — ladder** | `java -version` → Java hello world → evaluate → Clojure web server | `curl localhost:PORT` returns `Hello, World from Clojure` |

**Transition rule.** All of the phase's gates green **and** records complete. Then: write a phase
summary in `RESULT.md` (gates, passes, decisions, interventions). If the phase so far needed **zero
human interventions**, the agent opens the next phase itself; if not, it stops at the summary and a
human reviews. Autonomy is earned per phase, not granted once.

The ladder's rungs gate each other and nothing crosses: no Clojure jar is touched before Java hello
world's gate is green; the evaluate rung is a pass like any other (its research artifact is the
deliverable).

## Rules for code — reduce new code, compose existing crates

The tree already contains most of every answer. The extraction history (the `*_EXTRACTION` docs, the
pure `akuma-syscalls-*` crates) exists precisely so a new capability is composition before it is
code. The signal bookkeeping already sits in `akuma-syscalls-signal`; the W^X checks in
`akuma-syscalls-mem::wx`; the fault path in `akuma-exceptions`; maps rendering, gated, in
`akuma-procfs`; auxv in `akuma-elf-exec/src/stack.rs`. Denver's job is to find, connect, and fill
only the holes.

- **R1 — Research first, always.** `denver/docs/research/<pass>.md` before any code, with: crates,
  files and functions found (named, with paths); what is reusable as-is; what is missing; the
  composition plan; a new-LOC estimate.
- **R2 — Compose, don't fork.** Call existing pure crates from `akuma-jvm-kernel`. If an existing
  crate lacks a decision, add the decision *there* — a small pure function, host-tested — never a
  forked copy.
- **R3 — Extract when isolated.** New pure logic lives in a syscall-family crate
  (`akuma-syscalls-*`) or the capability crate, host-tested (`cargo test -p`), `#![no_std]`, no
  unsafe; front crates `#![forbid(unsafe_code)]`; workspace lints stay clean.
- **R4 — Arms are thin.** Machine-half syscall arms do ABI glue only: validate, `copy_to_user`,
  errno. Decisions live in pure crates.
- **R5 — Budget the LOC.** If an implementation passes ~2× the estimate in R1, stop: the change is
  fork-shaped, re-research before writing more.
- **R6 — Patches land behind features.** Existing crates gain per-arch features, off by default;
  `akuma-jvm-kernel` maps its feature names onto them (the way `console` propagates today). The
  repo's default build never changes.
- **R7 — No middleware.** No hook tables, no refusal layers: a capability is code in the kernel plus
  a feature, and a policy change is the policy changed.
- **R8 — Nothing lands unproven *as a record*.** The gate record commits on gate evidence only,
  and the driver commits it. Work in progress rides a separate track: every ~15 minutes, while what
  you touched still builds (`cargo build -p <crate>` / the kernel target), commit `denver(wip):
  <gate> - <what>` with `--no-verify` and push to `litter`. A WIP commit protects the work; it is
  never evidence and never closes a pass. Diagnostics passes (`/proc` maps, named faults) are
  first-class capabilities, not chores.

## Decisions have precedents (R9)

Before any decision the code rules don't settle — naming, where a decision lives, which errno,
whether to insert a pass ahead of another — **search for the precedent first**: `rg` through
`docs/archive/` and `docs/reference/`, then the other kernels (`shanghai/`, `taoxichuan/`, the
`akuma-tokio-kernel*` crates, `osaka` if it exists in the tree). Cite what you found in the research
artifact and the LOOP_LOG entry (`decided X — precedent: <doc/path>`). **No precedent found is itself
a finding**: record it, and prefer the smallest deviation from the nearest precedent. This repo has
~400 archive docs and four kernels; the answer already exists more often than not, and an uncited
novel decision is how a fork begins.

## The escape hatch

The loop has exactly one way to ask for help, and it is a file: `denver/ESCALATE.md`, with `QUESTION:`
and `CONTEXT:`. Write it when the stall protocol is exhausted, or when a decision is beyond the
plan's authority (policy, ordering, spend), leave `NEXT GATE:` pointing at the same gate, and stop
cleanly. The driver script stops the loop on it. Then one of two recoveries:

- **A human answers** — edit the file, rerun `ESCALATE_ANSWERED=1 sh denver/autoresearch.sh 1 <gate>`;
  the script archives the answered escalation to `denver/docs/archive/escalations/` and continues.
- **A stronger model answers** — `ESCALATE_MODEL=<model> sh denver/autoresearch.sh 1 <gate>` reruns
  the same gate with the model override (goose reads `GOOSE_MODEL`/`GOOSE_PROVIDER` from the
  environment); escalate again if it also stalls.

Every escalation and every redo lands in `RESULT.md` and in
[`AUTORESEARCH_WORK_LOG.md`](../archive/AUTORESEARCH_WORK_LOG.md) — asking for help is a loop result,
not a loop failure.

## Stall protocol

Three hypotheses without a verdict is a stall, and a stall is a requirement discovery:

1. Write the stall into `LOOP_LOG.md`: what was believed, what was measured, what was wrong.
2. Ask what diagnostic capability is missing and insert a pass for it ahead of the blocked one
   (maps before signals was the attempt doc's own advice — the ordering is a hypothesis too).
3. Re-rank `REQUIREMENTS.md` with the reason recorded. The plan's order was a guess; the stall is
   the data.
4. If the blocker is the plan itself — a wrong assumption, a lesson that teaches the wrong thing —
   log it as a lesson defect, quote it, and fix the living lesson, exactly as the shanghai-01 brief
   asked the agent to report every place the README was wrong.

## End of a session

`LOOP_LOG.md` tail is the next agent's orientation. `RESULT.md` current. Everything proven is
committed and pushed to `litter` by the driver; WIP went up on the 15-minute cadence. One line:
`NEXT GATE: <name>`. If a session must end with unproven work that never made a WIP commit, the
note says so plainly — unpushed work with no note is the one unforgivable exit.
