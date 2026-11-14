# Lessons — general method, with the denver autoresearch run as the worked example

> The expanded version of this list — one section per lesson, with the distilled
> bullets — lives in the project README: [`../../README.md`](../../README.md).

Six living lessons distilled from the denver autoresearch experiment (2026-11-13 →
2026-11-14, governed by [`docs/runbooks/AUTORESEARCH.md`](../runbooks/AUTORESEARCH.md)).
Each lesson file states its rules in general voice up top and carries the Akuma-specific
worked example below; the `Record` sections append per gate, so the files grow as the
evidence does. Every rule in every lesson is proven by a captured gate log or a recorded
decision — nothing here is doctrine without evidence.

## The lessons

| # | lesson | the one-liner | worked example |
|---|--------|---------------|----------------|
| L1 | Collecting requirements | the workload is the spec — run it, catalogue every failure as a capability gap with evidence, rank by the workload's own order, never by taste | [`L1-collecting-requirements.md`](L1-collecting-requirements.md) |
| L2 | Writing probes | the instrument is part of the experiment — prove your tools before trusting any result; a verdict that depends on how you captured it is not a verdict | [`L2-writing-probes.md`](L2-writing-probes.md) |
| L3 | Building a platform crate | port the proven stack, don't fork it — and verify the ground you're standing on before writing code | [`L3-building-a-kernel-platform-crate.md`](L3-building-a-kernel-platform-crate.md) |
| L4 | The loop | measure the loop that does the measuring — cheap cycles, honest failures, records before polish | [`L4-the-loop.md`](L4-the-loop.md) |
| L5 | Interrupt when it matters | the first path found is not the path required — ask "can we go around?" before chasing; a well-aimed interruption is the cheapest tool in the box | [`L5-interrupt-when-it-matters.md`](L5-interrupt-when-it-matters.md) |
| L6 | Find the feature flag | before you build it, grep for it — "not implemented" is a configuration question before it is an implementation task | [`L5-find-the-feature-flag.md`](L5-find-the-feature-flag.md)¹ |

¹ filename predates the 2026-11-14 renumber (this lesson moved from L5 to L6 when the
interrupt lesson landed); the file keeps its name until its cross-references
(`REQUIREMENTS.md` G15 row, the experiment-results archive, probe-script comments) are
moved with it.

## How L5 and L6 relate

Both are the check-before-build discipline. **L5 is the judgment half** — pause the
momentum, interrogate whether the obstacle exists at all, and let human intuition aim
the shortcut an agent grinding a self-imposed path will never see. **L6 is the
mechanical half** — building seems necessary now, so diff the configuration of the
system that works against the one that doesn't before writing anything. Read in order:
first ask whether the work exists, then ask whether the work is already configured in.

## House pointers

- Governing runbook: [`../runbooks/AUTORESEARCH.md`](../runbooks/AUTORESEARCH.md)
  (the loop, R1–R9, file ownership — these lessons are runbook-owned living docs)
- Requirements table the lessons feed: [`../REQUIREMENTS.md`](../REQUIREMENTS.md)
- The escalation channel (L5's subject): `denver/ESCALATE.md` while an experiment runs,
  archived to [`../archive/escalations/`](../archive/escalations/) once answered
- Evidence for every rule: `denver/gates/*.log` and each lesson's own `Record` section
