# Denver autoresearch: work log (started 2026-11-13)

**What this is:** the log of the autoresearch effort *itself* — issues in the loop, script fixes,
runbook and protocol changes, escalations and their answers, and what each was believed, measured,
and changed. Newest at the bottom, diary style, per
[`BUILDING_KERNEL_WITH_LOCAL_AI_PAIN_AND_SUFFERING.md`](../../..//docs/archive/BUILDING_KERNEL_WITH_LOCAL_AI_PAIN_AND_SUFFERING.md).
This is not the gate record (`denver/RESULT.md`) and not the capability record
(`denver/docs/LOOP_LOG.md`); it is about the machinery those two run on. Every entry names the fix,
so the next agent that hits the same wall finds the answer here first (R9 applies to the machinery
too).

## 2026-11-13 — the machinery exists

**What was built.** The plan moved from `docs/archive/` to `denver/docs/archive/`
(`KERNEL_HOWTO_BUILDING_CUSTOM_PLATFORM_KERNEL.md`, relative links fixed for the new depth). Around
it: the runbook (`denver/docs/runbooks/AUTORESEARCH.md` — session loop, phases, R1–R9, stall
protocol, escape hatch), the session brief (`scripts/kvm/goose/denver-01-autoresearch.md`), and the
driver (`denver/autoresearch.sh`): one invocation = one or N sessions = one gate each, next gate from
the `NEXT GATE:` line of `LOOP_LOG.md`, judged on bookkeeping only.

**Believed: sh is sh.** Found while testing the driver on a throwaway branch: `grep -c` under
`set -e` with an `|| echo 0` fallback double-prints (`0` from grep, `0` from echo) and silently
corrupts every arithmetic comparison downstream; `a && b` at statement level aborts the whole script
when `a` fails (POSIX `set -e` applies to the list); and a stale `rc` from one session leaked into the
next because shell functions without `local` share globals. Fixed: `count()` captures into `n` first;
every `&&` guard became an `if`; `rc=0` reset before each `wait`. *Lesson: `sh -n` proves syntax, not
behavior — dry-run on a throwaway branch is part of writing the script.*

**Believed: the driver should trust goose's verdicts.** Rejected on precedent: the Qwen experiment
(`INSTALL_SCRIPT_QWEN_EXPERIMENT.md`) showed an agent judging its own work tunes on the test set. The
driver greps captured probe output for `RESULT PASS` and never asks goose whether a gate passed;
goose's word is not evidence. Precedent for the marker convention: `MWPROBE RESULT PASS` in
`shanghai/mw_gate.sh`.

**Commit policy.** The driver commits and pushes to `litter` itself: docs and records with
`--no-verify`; code only after targeted `cargo test -p` for every crate the session touched, then
also `--no-verify` — repo-wide hooks would check the whole workspace every session, and the loop pays
per token. Checks red = the code stays uncommitted and the loop stops for a human. Push failure is a
logged warning, not a stop (checkouts without the `litter` remote still work; commits stay local).

**The escape hatch.** One way to ask for help: `denver/ESCALATE.md` (`QUESTION:` / `CONTEXT:`). The
driver stops on it; a human answers and reruns with `ESCALATE_ANSWERED=1` (the answered file is
archived to `denver/docs/archive/escalations/`), or a stronger model redoes the gate via
`ESCALATE_MODEL=<model>` (goose reads `GOOSE_MODEL`/`GOOSE_PROVIDER` from the environment).
Escalations are loop results, not loop failures — they count in `RESULT.md` next to interventions,
and the per-phase autonomy rule (zero interventions = the agent opens the next phase itself) treats
them as the same currency.

**R9, decisions have precedents.** Added to the runbook after noticing the runbook itself kept
citing precedents to justify its own rules: the driver's fairness rule cites the Qwen experiment, the
marker convention cites `mw_gate.sh`, the brief format cites the shanghai-01 brief. The agent gets the
same obligation: every decision cites the doc, the crate, or the kernel it came from, and *no
precedent found* is recorded as a finding, not passed over in silence.

## 2026-11-13 (later) — the driver fired a real goose during a test

**What happened.** Verifying the escalation path by hand: created a leftover `ESCALATE.md`, ran the
driver, and went to coffee. It was not a dry run — the loop picked `NEXT GATE`, fired a real goose,
and sat there until the outer timeout killed the shell (the goose itself had already failed fast; no
guests were harmed, and `goose serve` on the box is infrastructure, untouched).

**What was wrong.** The escalation check lived *after* the goose run inside `session()` — correct for
an escalation filed mid-session, invisible to a leftover one. A closed loop must refuse to open a
session on an unanswered question, exactly as it refuses without goose on PATH.

**Fixed.** The loop-level check now runs before `session()`: open `ESCALATE.md` → exit 5, nothing
fires; `ESCALATE_ANSWERED=1` → the file is archived to `denver/docs/archive/escalations/<date>-<gate>.md`
and the redo proceeds. The mid-session check stays (commit what is in the tree so `litter` shows the
question, then stop).

**Second bug, found by the first.** The timed-out test left the lock behind, and the next run's
`die()` removed *someone else's* lock on its way out — `die` runs `rm -rf "$LOCK"` unconditionally,
so a run that never owned the lock deletes it and lets two drivers run concurrently. Fixed: the
lock-guard exits without touching the lock; only the owner cleans up (trap or explicit exit).

**Lesson for the log.** The session loop's own escape hatch needed the same property the loop
teaches: diagnostics before action. Test drivers with `DRY_RUN=1` on a throwaway branch — and a test
that can fire a real session should set the env that proves it cannot.

## 2026-11-13 (evening) — the kimi auth error: the driver did not pin its endpoint

**What happened.** The first real launch died on a kimi auth error. The driver sources
`/etc/goose.env` (which sets `GOOSE_MODEL=glm-5.3-flash`) — but that file carries no endpoint host,
so the provider host fell through to `~/.config/goose/config.yaml`, which carries *kimi* defaults
(`api.kimi.com`, model `kimi-for-coding`). Result: the GLM model name sent to kimi's endpoint; auth
refused. Believed: sourcing the env file is enough. Measured: the model and the host come from
different places, and they disagree.

**Fixed.** The driver pins the whole quartet before the first session — `GOOSE_PROVIDER=openai`,
`GOOSE_MODEL=glm-5.3-flash`, `OPENAI_HOST=https://api.z.ai`,
`OPENAI_BASE_PATH=api/coding/paas/v4/chat/completions` — each still overridable from the
environment. *Lesson: a config cascade (env file → config.yaml → ambient env) is a failure
convention waiting for a launch from the wrong shell; a batch driver pins what it depends on.*

## 2026-11-13 (later still) — WIP cadence and the quota wait

Two protocol changes, both pragmatic, both logged here because they *amend* rules written earlier
the same day:

- **WIP commits every ~15 minutes.** R8 originally said "nothing lands unproven," read as a total
  commit ban. Amended to "nothing lands unproven *as a record*": the agent commits `denver(wip):
  <gate> - <what>` with `--no-verify` and pushes, on a ~15-minute cadence, only while the touched
  packages still build. Reason: an overnight session that dies at minute 90 without commits loses
  more than a WIP commit ever risks; a WIP commit protects the work and is never evidence, and the
  driver's gate commit remains the only one that closes a pass. *Believed: frequent WIP commits
  muddy the history. Amended: muddied history is what the docs/records commits are for; a lost
  evening of agent work is worse.*
- **The z.ai quota wait.** In batch mode, a quota death is a wait, not a failure: the driver reads
  the last 50 log lines for the quota patterns (429, quota, rate limit, billing, ...), sleeps
  `QUOTA_WAIT_SECS` (default 600s), and retries the *same* session — the session budget and the
  failure counter are untouched. `QUOTA_MAX_RETRIES` (default 12) bounds the total waiting; past it
  the loop stops for a human. *Believed: goose failures are model failures. Amended: on a shared
  quota, goose will die at 3am with a 429, and a driver that counts that as "goose failed twice in
  a row" stops by breakfast for no reason.*

## 2026-11-13 (session denver-01, PHASE-0-SCAFFOLD) — three machinery findings from the first real session

**1. The driver did not stage the root manifests.** The scaffold gate's evidence includes a
`members` line in the root `Cargo.toml` (and its `Cargo.lock` churn); the staging list covered
only `denver akuma-jvm-kernel crates scripts/...`, so the driver would have committed a tree
that cannot build its own kernel (a non-member `-p denver` fails). Fixed: staging now includes
`Cargo.toml Cargo.lock`, and the package-test loop skips root-manifest-only paths (dirname `.`)
so a members line does not pay a root-kernel host test. *Believed: gate evidence lives under
gate-named paths. Measured: workspace membership is part of the evidence. Lesson: the staging
list must follow what makes the tree build, not what the gate directory happens to contain.*

**2. The box's toolchain does not enforce the repo pin, and never could build aarch64.** No
rustup: `akuma-dev.env` puts standalone `/usr/local/rust` (cargo 1.101.0-nightly) on PATH, and
its rustlib carries only `x86_64-unknown-{none,linux-musl}`. Consequences, recorded in
`RESULT.md` so nobody rediagnoses: `rust-toolchain.toml` is not honored on this host (the pin
exists for Mac-vs-ryzen parity; shanghai's proven stack was built with this same compiler), and
the aarch64 default build is not runnable here — the R6 default-build surface on this box is the
x86_64 + host surface. Not fixed, because nothing is broken: recorded as a machine fact.

**3. Guests are impossible until the host reboots with `kvmctl`.** The first real boot probe
failed at `open("/dev/kvm0")` ENOENT — exactly the documented semantics (kvm.md line 192; the
device arms at boot only, `amd64/src/main.rs`). Firecracker shares the dependency; there is no
QEMU anywhere on the box. The session escalated (`denver/ESCALATE.md`) rather than improvising:
rebooting the machine that runs the session is machine policy, not a plan decision. *Lesson: a
session brief that says "boot guests with whatever the machine has" should be read to include
"and stop at the escape hatch when the machine has nothing" — three hypotheses were spent
confirming a fact one doc line already stated.*

**4. The root `.gitignore`'s `*.log` eats the gate logs.** `git add -A denver` silently skipped
`denver/gates/PHASE-0-SCAFFOLD.log` (pattern `*.log`), so the driver would grep the on-disk file
while the pushed record carried no proof. Fixed in the same staging block: `git add -f
denver/gates` before the commit. *Lesson: an ignore pattern written for build noise and an
evidence directory written for records disagree silently; `git status --short` the gate log's
absence would have shown — check what the record commit actually captured, not only what the
disk holds.*

**5. The escalation archive can be overwritten by its own answer cycle.** The loop check
archives an answered escalation with `mv denver/ESCALATE.md
denver/docs/archive/escalations/<date>-<gate-slug>.md`; a second escalation on the same gate
on the same day (this phase-0 review, after the reboot escalation) mv'd onto exactly that
path, replacing the earlier archive in the worktree. Fixed while filing the review: when the
target exists the archive lands beside it with the driver pid
(`<date>-<slug>-<pid>.md`). Applied mid-run deliberately: the block sits inside the already-
parsed `while` body and the escalate path exits before any unparsed byte of the script
executes; `sh -n` green. *Believed: the archive step is bookkeeping and idempotent. Measured:
a same-key second record silently replaced the first. Lesson: every mv-into-archive in the
machinery needs an exists-check — evidence moves must never be overwrites.*

## 2026-11-14 (late) — quota deaths, the operator checkpoint, and the paperwork close-out

- **Two z.ai quota deaths, no evidence lost.** The JAVACLASSLIB + fault-signal-port stretch
  was cut off at the `38f16b2d` checkpoint ("unverified, no gate run", 17:17); session 20
  (JAVAC) died right after `RESULT PASS` (wall 82 s, `denver/gates/PHASE-3-JAVAC.log`). The
  driver stayed down (wifi outage — see the wifi dormancy audit, commit `29687c9c` — plus the
  weekly quota); the operator kept the gate + attempt-1 logs (`c3541214`) and checkpointed
  REQUIREMENTS (G13 closed, G14 SIGRAX ranked first, G15 closed), lessons L1–L4 records,
  STATE and COST_LEDGER (`b2f4a5dc`) — whose commit message names the pieces still missing.
- **The missing pieces, now written (paperwork session, operator-directed: records only, no
  rust edits, no kernel builds):** RESULT.md entries for PHASE-2-NULLFIX (never written when
  it ran), PHASE-3-JAVACLASSLIB and PHASE-3-JAVAC; the phase 2 and phase 3 summaries (the
  transition rule's artifacts — phase 2's had also been skipped when phase 3 opened);
  LOOP_LOG entries for sessions 18–20; the single final `NEXT GATE: PHASE-4-SIGRAX` line;
  STATE.md regenerated from the corrected LOOP_LOG; a COST_LEDGER note untangling the
  session-name collision (three goose sessions named `denver-PHASE-3-JAVACLASSLIB` — the
  19:12 one is the JAVAC gate).
- **Open work left in the tree, noted not committed.** The working tree carries uncommitted
  G14 debug instrumentation in `crates/akuma-tokio-kernel-amd64/src/faultsig.rs`
  (`[SIGRAX-DLV]` / `[SIGRAX-DBG]` register dumps in `deliver` / `sync_sigreturn`) —
  diagnostic print instrumentation for the rax-restore bug, not built, not gated, committed
  nowhere. Left uncommitted by agreement; PHASE-4-SIGRAX must build and judge it under its
  own rules (R8: a WIP commit is never evidence) before it is anything but WIP. With G14
  subsequently closed by the operator (`180caf82` — the rc=0-want-99 datum was the committed
  sigprobe binary's own bug: it stored the rax edit in the rcx slot), the instrumentation's
  target is known-moot. Untracked
  `userspace/nca/` and two dirty vendored submodules (`rumpkernel/src-netbsd`,
  `tcc/tinycc`) predate the denver loop and are left alone.
- **Where the experiment stands at close:** G1–G4, G6–G13, G14, G15 GREEN (G14 closed by
  `180caf82` — probe-binary bug, PHASE-4-SIGRAX moot); G5 deferred (perf pass); suspects
  unranked. Capability work is
  done (javac compiles in the guest; Clojure/Scala/Groovy run on the faultsig+shell rig per
  the operator's measured runs); what remains is polish.
