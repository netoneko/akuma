# denver

<!-- PREFACE: placeholder — written together next; the lessons curriculum below is locked. -->

The lessons below are the general method distilled from the denver autoresearch run
(2026-11-13 → 2026-11-14). Every rule is proven by a captured gate log or a recorded
decision — nothing here is doctrine without evidence. The links go to the living lesson
files, which carry the full worked example and the per-gate `Record` trail.

## The lessons

### L1 — Collecting requirements

**The workload is the spec.** Don't guess what to build — run the thing you actually
want to work, and let its failures write the list.

- Run it on your *best* system first — the richest kernel fails latest, and where it
  fails is your gap list
- Run the same binary everywhere you can (A/B) — each system's gap list starts where
  its own run stopped
- Rank gaps by the workload's own order of operations, never by taste; a gap you can't
  reach yet must not outrank one you can
- "Suspects" (unexplained errors) stay hypotheses until something that actually reaches
  them dies naming one
- The list is alive: fix one gap and the whole order may shift — re-read it after
  every pass
- "Not found in the repo" ≠ "doesn't exist" — ask before you build a substitute
- Evidence comes from captured logs, never from memory

→ worked example: [`docs/lessons/L1-collecting-requirements.md`](docs/lessons/L1-collecting-requirements.md)

### L2 — Writing probes

**The instrument is part of the experiment.** Prove your measuring tools before you
trust any result — a verdict that depends on how you captured it is not a verdict.

- The verdicts lie — in many ways: pipe buffering drops output, wrong constants in
  checks, unset variables, parsing by position instead of content, checks that read
  the wrong line
- A checker that shares the author's assumptions verifies nothing — the receiver is
  the verifier
- Before recording a "divergence", ask what your probe would print on a system doing
  the right thing — a channel that can't see the thing reports blindness, not behavior
- The number the caller performs is the contract — disassemble the actual artifact,
  don't trust your memory of it
- Attribution before dismissal: the log line that appears exactly when things break is
  not "noise"
- Preserve the input you actually delivered (checksum it); never modify a proven image
  in place
- A healthy system can still starve one round trip — aliveness is baseline + final
  poke + retry, not a single ping
- The verdict gate must not be stricter than the arms feeding it
- The instrument can *be* the failure — and when it reports the impossible, suspect
  its own map of the world first

→ worked example: [`docs/lessons/L2-writing-probes.md`](docs/lessons/L2-writing-probes.md)

### L3 — Building a platform crate

**Port the proven stack, don't fork it** — and verify the ground you're standing on
before writing code.

- Orient on branch/reality first: a plan's reading list is a dependency list — verify
  it exists where *you* are
- Merge the proven stack, don't copy pieces — partial copies are fork-shaped
- A verbatim code move is portable exactly as far as the ownership model it assumes —
  understand the model, not just the code
- Port the precedent, don't re-derive: an existing working module is the source to
  port from, not a design to reinvent
- Negative evidence from a listing isn't evidence ("not in `ls`" ≠ "doesn't exist") —
  check by operating
- Rig hygiene before diagnosis: a stale leftover answering EBUSY reads as "the system
  is broken"

→ worked example: [`docs/lessons/L3-building-a-kernel-platform-crate.md`](docs/lessons/L3-building-a-kernel-platform-crate.md)

### L4 — The loop

**Measure the loop that does the measuring** — cheap cycles, honest failures, records
before polish.

- Name which cycle you're measuring (probe cycle vs. change cycle) or the number lies
  by ambiguity
- Diagnostics before fixes — probes that read the system beat any guessed fix
- Preserve the failed run before re-running — a verdict that overwrites its own
  evidence can only be repeated, never learned from; the honest FAIL is often the most
  valuable event
- Records before polish: when time/context is running out, write the records first —
  polish is the only thing safe to lose
- Named ≠ understood — if one cheap run can convert "named the cause" into "measured
  the mechanism", spend the run
- Measure the clock too: a measurement that can silently go negative will lie exactly
  when something depends on it
- Interventions are data — human corrections are findings, not interruptions

→ worked example: [`docs/lessons/L4-the-loop.md`](docs/lessons/L4-the-loop.md)

### L5 — Interrupt when it matters

**The first path found is not the path required.** Ask "can we go around?" before
chasing — a well-aimed interruption is the cheapest tool in the box.

- Both humans and AIs get caught up chasing the thing they found first; the discipline
  is interrogating the path before the cost scales
- Agent side: ask before building — "does this already exist?" costs one question; a
  substitute costs hours and ships a subtle wrongness. Escalation is not failure
- Human side: interrupt on purpose — momentum is not a reason; an agent deep in a
  self-imposed path won't ask for directions on its own
- The detour can still pay (lessons, instruments) — but it was work on a false
  premise; the same hours after one question produce the gate directly

→ worked example: [`docs/lessons/L5-interrupt-when-it-matters.md`](docs/lessons/L5-interrupt-when-it-matters.md)

### L6 — Find the feature flag

**Before you build it, grep for it.** When something answers "not implemented", the
first question isn't "how do I implement it" — it's "does it already exist behind a
switch?"

- Read the numbers, not the stack — catalogue what's actually missing before
  theorizing
- Check whether a sibling/simpler system already serves it, and diff the
  configurations to find the switch
- Being *minimal on purpose* is sound gate-by-gate and wrong as a final answer —
  periodically diff what you've declined against what's available
- Honest cost accounting: admit what you turned on but didn't test, and admit when you
  skipped a process step

→ worked example: [`docs/lessons/L5-find-the-feature-flag.md`](docs/lessons/L5-find-the-feature-flag.md)¹

¹ filename predates the L5/L6 renumber (this lesson moved down when the interrupt
lesson landed); the file keeps its name until its cross-references move with it.

## How L5 and L6 relate

Both are the check-before-build discipline. **L5 is the judgment half** — pause the
momentum, interrogate whether the obstacle exists at all, let human intuition aim the
shortcut. **L6 is the mechanical half** — the work seems necessary now, so diff the
configuration of the system that works against the one that doesn't. Read in order:
first ask whether the work exists, then whether it's already configured in.

## House pointers

- Lessons index (file map, renumber note): [`docs/lessons/README.md`](docs/lessons/README.md)
- Governing runbook (the loop, R1–R9, file ownership): [`docs/runbooks/AUTORESEARCH.md`](docs/runbooks/AUTORESEARCH.md)
- Requirements table the lessons feed: [`docs/REQUIREMENTS.md`](docs/REQUIREMENTS.md)
- The experiment's records and captured evidence: [`RESULT.md`](RESULT.md), [`docs/LOOP_LOG.md`](docs/LOOP_LOG.md), [`gates/`](gates/)
