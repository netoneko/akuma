# AI will write 600 bugs and I will write 600 more — presentation script

**Kirill Maksimov · 2026-08-17**

**Status:** draft 16. The *content* of a talk plus speaker notes.

**Rendered deck:** [`bootstrap/public/600-bugs/index.html`](../../bootstrap/public/600-bugs/index.html)
— 13 slides, keyboard-navigable (↑/↓, Home/End), self-contained apart from
`lock_in.jpg` beside it. Served at `/600-bugs/`. Served from `bootstrap/public/`, so it is reachable from the
VM's own httpd. **This file stays the source of truth**: edit the script here, then
mirror into the HTML.

**Subject: the workflow that actually built this thing**, and what AI-assisted
development looks like in practice.

Deliberately **not** a numbers talk. Drafts 1–6 were organised around ten
"lessons" derived from line counts and bug densities; that framing is dropped —
the statistics live in [`LINE_COUNT_ANALYSIS.md`](LINE_COUNT_ANALYSIS.md) where
they belong, and the deck now shows the loop that produced them.

**Slides carry headlines and evidence, not paragraphs.** If a slide below runs
past ~8 lines of content it is still too long. The prose under each slide is
speaker material, not slide text.

### Changes from draft 15

| Change | Reason |
|---|---|
| **All ten "Lesson N" slides deleted** | Per review — retrofitted and weak. The density tables, robustness test and retraction stay in `LINE_COUNT_ANALYSIS.md` § Stat 6 |
| **Four workflow slides added** (goal → audit → probe → A/B) | Per review. This is the actual method, stated as a loop |
| **AI-assisted development slide added** | Per review: cognitive docs and project-specific intuition, from [`DEVELOPMENT_PRACTICES_REVIEW_AND_ASSESSMENT.md`](DEVELOPMENT_PRACTICES_REVIEW_AND_ASSESSMENT.md) §3–§4 |
| **"The records are the product" kept as slide 10**, reframed on how much is minable from one full OS cycle | Per review — the one surviving idea from the lessons draft. Now carries the recurring-defect-shape finding, which is the most actionable thing in the whole archive |
| **Slide 09 rewritten** — reviewing Claude's diff, a 4.5k-line file, one function; no technical detail, calmer tone | Per review |
| **"Three things I deleted and had to put back" cut** | Per review. The deck now opens straight into the workflow; the git archaeology stays in this doc's Background note below |
| **Slide 06's examples removed entirely** — the slide now just makes the case that instrumentation and data-flow tracking always pay off | Per review. The example material stays findable in Background |
| **Slide 01 reframed** — project credentials dropped; the point is now that more generated code means more checks, and the same shift makes those checks cheap | Per review ("remove glazing") |
| **Asterinas comparison moved up to slide 02** and expanded — safe Rust, the 14% TCB, and their scoping of formal verification | Per review. It now *sets up* the workflow act instead of trailing it: the best-resourced team in the peer group also reached for practical tools past the part proofs can cover |
| **Slide 02 renamed "Competitive landscape"; the AI-policy slide moved up to 03** | Per review. Both landscape slides now sit together as Act I, so the deck establishes where it sits and who bans what *before* the method rather than after |
| Peer group, AI policy, `go build` defeated | Kept per review |
| Content per slide cut hard throughout | Per review — "way too much content" |
| **Title set: "AI will write 600 bugs and I will write 600 more"**, subtitle *"at some point you gotta do some real engineering"* | Per review. Slide 01 now leads with it; 622 documented fixes is the number behind it |
| **Slide 10 renamed "Learn from history"**, the unsubstantiated "almost nobody mines their own" removed, and five real commit messages added | Per review |
| **"one maintainer" dropped** from the subject line, the AI-policy row and slide 10 | Per review — "no one cares" |
| **Deck rendered to HTML** and both files renamed `600_BUGS_ANNIVERSARY` | Per review |
| **Title slide split in two** — 01 is title + subtitle + meme only, 02 carries the "checks got cheap too" argument | Per review. The image needs to land on its own before the argument starts |
| **The commit-message block became its own slide, 10, titled `mood`** | Per review — eleven lines was too much to share with the docs slide. `mood` is itself one of the commit messages (2026-02-22, ×3) |
| **Byline added** — Kirill Maksimov · 2026-08-17, on slide 01 and this doc's header | Per review |
| **Negative framing audited and flagged** — see § "Flagged: negative framing" | Per review. Eight instances found across both files; two flagged as rewrite candidates, six defended. Nothing changed yet |

---

## Thesis

> **The problem with writing throwaway software is that people don't actually
> throw away software.**

Thirteen slides. The opener states the trade, Act I places the project, Act II is
the workflow, Act III is how it's built.

**Title:** **AI will write 600 bugs and I will write 600 more**
**Subtitle:** *at some point you gotta do some real engineering*

---

## Opening

### 01 — AI will write 600 bugs and I will write 600 more

> *at some point you gotta do some real engineering*

**Kirill Maksimov · 2026-08-17**

*Slide is the title, the subtitle, the byline and the meme. Nothing else.*

*Speaker:* the number is real and it's the next slide. Let the image land first.

---

### 02 — The checks got cheap too

**622 documented, dated, fixed.**

More code generated means more code to check. **But the same shift that made the code
cheap makes the checks cheap** — probes, tripwires, self-tests, boot suites, A/B runs
on real workloads, all cheap enough to write per-bug and leave in place.

**So the checks stop being a budget item you ration, and start being the default
response to every change.**

*Speaker:* that's the whole trade. Not "can it write a kernel" — it can — but that the
volume it produces has to be met with a matching volume of checking, and fortunately
that got cheap at the same time and for the same reason. The workflow act is what I
actually spend the checking budget on.

---

## Act I — The landscape

### 03 — Competitive landscape

| | **Redox** | **Asterinas** | **Akuma** |
|---|---|---|---|
| since · people | 2015 · team + nonprofit | ~2022 · 50+, 3 universities | 2026 · **1** |
| funding | EU NGI grants | Ant Group + Intel | none |
| high-water | COSMIC desktop, packages | nginx **faster than Linux**, Firefox, Redis at parity | Redis + official image, Go, rustc, llama.cpp |
| builds itself | not yet | not established | **yes** |

**Asterinas is the one worth studying.** A *framekernel*: all `unsafe` is confined
to one library (OSTD, ~15k lines, **14% of the kernel** — about the size of seL4's
verified core), so every service above it is written in **100% safe Rust.**

**And they drew a hard boundary around formal verification.** They verify the tiny
TCB for memory safety — tractable at 15k lines — and then say memory safety is
effectively solved, so focus shifts to *bugs beyond safety*. For those they reach
for **model checking and tests**, not proofs. Logic-level verification is
explicitly aspirational.

*Speaker:* this slide sets up Act II. The team most invested in formal
verification, with the funding and the university muscle to do it, scoped proofs to
the 14% where proofs actually work and used practical tools — model checking, real
workloads, tests — for everything else. That's the argument for the workflow: I have
no formal verification and no proofs, so the practical half is all there is, and it
turns out that's what the serious people spend most of their effort on too.

Two honest notes: Asterinas is bigger, faster, better funded and more rigorously
verified than this project **and hosts no compiler** — capability is not one axis.
And where they overlap me on Redis, they publish throughput numbers and I have
none. "Runs it" and "runs it at parity" are different claims.

**Sourcing:** the safe-Rust and 14%-TCB claims are from their USENIX ATC'25
abstract and their own blog; the verification-boundary framing is from the blog and
LWN's coverage. A stronger verbatim version of the "formal verification says
nothing about implementation correctness" line may exist in their conference talk —
**unverified, do not quote it as theirs** until the video is checked.

---

### 04 — Same pressure, opposite answers

| | AI-contribution policy |
|---|---|
| **Redox** | **banned**, Feb 2026, enforced — LLM-labelled contributions closed on sight; bypassing it is a project ban |
| **Asterinas** | **welcomed and automated** — *"AI is welcome, but the human is responsible"*; ships an AI PR-review bot |
| **Akuma** | no policy. Built with AI from the start |

Same pressure — generation outpacing review capacity. Two structurally opposite
answers, both from funded, multi-institution projects.

*Speaker:* both policies legislate *who writes the code*. Neither addresses who
keeps it. With no second reviewer there's no bottleneck to legislate, which is
why the mechanised parts — lints that deny by default, a pre-commit hook stricter
than most CI — matter more here, not less. They are the review.

---

## Act II — The workflow

### 05 — Have a concrete goal: run X

**Compatibility with X, in order to run X.**

| target | what it forced |
|---|---|
| **redis** | `/proc/<pid>/*`, `MADV_FREE` honesty, TCP connect-state, short-write `writev` |
| **rust / cargo** | `MAP_SHARED` writeback, 128 KB argv, thread-group reaping, `getpriority` |
| **golang** | signals on a foreign signal stack, pidfd, `waitid` parentage, fork/exec |
| **llama.cpp** | lazy mmap, file-page eviction, heap-growth headroom |

**A real program is a specification you didn't write and can't argue with.**

*Speaker:* none of those columns is a list I had to invent. The program decides
what "done" means, it fails in a specific place, and it can't be negotiated with.
Pick the program and the roadmap writes itself.

---

### 06 — Audit before touching code

**Update the references and the diagrams first. Then reason about where the problem
could be.**

- Re-measure the project's own headline numbers. One audit opened by knocking a
  claimed **88.8%** down to **23.0%** — the gap was a bug in the profiler.
- Standing rule: **never trust a percentage without re-measuring.**
- Enumerate the candidate theories explicitly, before writing any code.

*Speaker:* this step exists because the most expensive sessions here all started
from a number that was wrong. Updating the reference docs and diagrams isn't
bookkeeping — it's how you discover your mental model and the code have diverged,
and it's much cheaper than discovering it from a fault address.

---

### 07 — Isolate theories with probes, then discredit them

**Build the smallest thing that can tell two theories apart. Kill theories until
one survives.**

**Adding instrumentation and tracking the data flow is always worth it.** Where a
value came from, who owns it, when it was freed — that work pays off even when the
theory it was built for dies.

Probes inspect *state*, not behaviour. Keep the good ones; leave them armed.

**The bug is almost never where the error appears.**

*Speaker:* the discipline is negative — you're not looking for evidence you're
right, you're building the cheapest instrument that can prove you wrong. The point
about instrumentation paying off is that it outlives the theory: probes built for a
hypothesis that turned out wrong were repeatedly still the thing that found the real
cause, or caught an unrelated bug later. That's why it's worth doing even when it
feels like a detour from the fix.

---

### 08 — Then A/B it on real workloads

**Any change to architecture or implementation detail triggers an A/B spree — on
real software, once the probes pass.** Same binary, one variable, zero tolerance.

1. host unit tests for the logic
2. a boot self-test hitting the real entry point
3. **same-binary A/B stress on real workloads** — in-VM `rustc -j4`, Go fork/exec
   stress, Redis, a container pull

Any *stuck* / *RECOVERED* / *PANIC* / *WILD* / *SPURIOUS* marker fails the run.

*Speaker:* "the tests pass" isn't the bar, because the tests were written by the
person who wrote the bug. The bar is that a real workload behaves identically
across two builds differing in one thing. Keep one workload as a control that
*should* be unaffected — if it moves, the theory was wrong.

---

## Act III — How it's built

### 09 — AI-assisted development: docs as cognition

**Every session starts with an assistant that has no memory of yesterday.** That
one fact determines the whole documentation system.

| tier | job |
|---|---|
| `runbooks/` | do X, expect Y — ends in **Verify** |
| `reference/` | current state only, each page graded **A / B / C** for how far to trust it |
| `archive/` | 200+ investigations, verbatim, **never rewritten** — including the wrong theories |
| agent-context files | the standing rules, so they're never re-litigated |

**Written to reconstitute project-specific intuition in a reader who starts from
zero.**

**The failure mode this guards against**, logged once in the archive: during what
should have been a mechanical port, two syscalls were **rewritten instead of
copied** — breaking an ABI. Caught and reverted, and named in the doc as exactly
that.

*Speaker:* this is the part I'd argue is genuinely new. The archive isn't history,
it's a cache of hard-won judgement — correctness rules learned the hard way, so
they stop being relearned. Two agent-context files existed in parallel at one
point, and the log shows a second model consulted mid-crisis for alternative
theories. The job drifts from writing code to curating the context that makes good
code likely.

On the rewritten-instead-of-copied bug, say the honest part out loud: it was caught
that once, and there is no way to know from the record how many similar
substitutions were not. That is the specific risk of this way of working, and no
amount of documentation tiers fixes it — only a diff you actually read.

---

### 10 — mood

```
2025-11-28  does not actually detect ram
2025-11-28  at least it still runs
2025-11-30  allocation is the root of all evil
2025-11-30  this is clearly getting out of hand
2025-12-27  aaaaaaaa CAT
2026-01-21  at least it boots
2026-04-02  god damn it x2
2026-04-03  why are we here? just to suffer?
2026-08-06  more tests and fixes, cargo remains undefeated
2026-08-11  curious case of nothingburger
2026-08-13  java crossover episode
```

The four tiers document the *reasoning*. The commit log documents the *state of
mind* — and it is still a record you can mine.

*Speaker:* not a joke reel — read in order it's a legible progression. "This is
getting out of hand" recurs four times across two months before it graduates to
"still out of hand"; the two `at least it` lines eleven days apart are the whole of
the first era; and the 2026-08 lines are from the period where every claim carries a
measurement. The tone survived, the rigour arrived. None of it was written for an
audience, which is exactly why it's usable evidence now.

*(`mood` is itself one of the commit messages, from the middle era.)*

---

### 11 — Reading the diff

Last August the build had been failing for days, filed under a label that turned
out to mean nothing.

I was reading the code as Claude edited it — the fault handler, about **4.5k
lines** — and one function looked like the one worth checking. It was.

That was luck with a good prior. So the same commit added a diagnostic that says
which path declined, and measuring the codebase started not long after.

*Speaker:* pattern-matching on a file I'd spent months in. It doesn't transfer and it
doesn't scale, so the useful part was turning it into something that prints the
answer next time. Reviewing the diff as it lands is the habit worth keeping.

---

### 12 — Learn from history

**A full OS development cycle is a surprisingly large dataset.** 1,547 commits, 196
investigation docs. What falls out of it:

- **622 distinct fixes** across 15 subsystems, itemised, dated, cross-referenced.
- **Two crisis windows, from commit volume alone** — and an independent
  churn-per-file measure lands on the same two months.
- **Recurring defect *shapes*.** ~3% of all fixes are the same underlying bug
  rediscovered in a different subsystem: stale address-space pointer ×3,
  lock-held-across-blocking ×4, readiness gaps ×6 across four files. The
  most-repeated shape: **a raw index outliving the thing it names.**

**That last bullet is worth more than any single fix.** It is a defect class you can
go look for on purpose.

*Speaker:* the sequel to slide 10 — once the diagnostic replaced the guesswork, the
records became something you could interrogate. Cross-joining lines against bugs
turned up a real signal, and also caught me out: my first cut of that analysis
concluded concurrency was the riskiest code per line, and re-deriving the grouping
reversed it. Same data, different filing, opposite answer.

---

## Closing

### 13 — `go build` defeated

```
akuma:~$ apk add go git
akuma:~$ git clone --depth 1 .../akuma-playground.git
akuma:~$ cd akuma-playground && rm -f *.c *.rs
akuma:~$ CGO=0 go build -v .
akuma:~$ ./playground
Hello from Golang on Akuma
```

Go 1.26.3 · cold build **112 s**, 38 stdlib packages · warm **14 s** · 2.3 MB
binary, runs.

The milestone table said *"in progress — crashes during compilation"* for **five
months.**

> **The problem with writing throwaway software is that people don't actually
> throw away software.**
>
> Pick a real program. Audit before you touch anything. Probe until a theory dies.
> A/B on the real workload. Write it down for whoever starts from zero tomorrow.
>
> None of that got harder. It got cheaper at exactly the same rate the code did.

*Speaker:* measured while these slides were being drafted, on a separate VM. The
cold number is the honest one — 112 s is mostly building the Go cache from nothing.

---

## Negative framing — audited and fixed

Scanned both files for the *not-X-but-Y* construction. Four instances were doing
rhetorical work rather than carrying information; all four rewritten:

| slide | was | now |
|---|---|---|
| 05 | *Not "improve the memory subsystem". Compatibility with X…* | *Compatibility with X, in order to run X.* |
| 07 | *…that work is never wasted, even when the theory turns out to be wrong* | *…that work pays off even when the theory it was built for dies* |
| 09 | *Not written for posterity — written to reconstitute…* | *Written to reconstitute project-specific intuition in a reader who starts from zero.* |
| 11 | *Nice feeling. Not a method.* | *That was luck with a good prior.* |

Left alone, because the negation is the information rather than the framing:
"the bug is almost never where the error appears" (slide 07), "'the tests pass' isn't
the bar" (slide 08), "rewritten instead of copied" (slide 09, literal), "there is no
way to know how many similar substitutions weren't" (slide 09), "capability is not
one axis" (slide 03), "neither addresses who keeps it" (slide 04).

**Check to apply if more appear:** if the long clause is spent on what the thing
isn't, rewrite it.

---

## Not in the deck

- **Any "lesson" framing**, and the whole line-count / bug-density analysis. It
  lives in [`LINE_COUNT_ANALYSIS.md`](LINE_COUNT_ANALYSIS.md) — § Stat 6 has the
  density-per-area join, the robustness test and the retracted concurrency claim. A
  talk leading with those tables argues about metrics instead of showing a method.
- **xv6 / seL4 comparisons.** Unrecognised; made the talk about size.
- **A feature tour.** The README does that.
- **Image bytes vs lines.** Different question — see
  [`../reference/build-profiles.md`](../reference/build-profiles.md).

## Open questions

1. **Slide 02 is now the longest in the deck.** Split into the table and the
   Asterinas argument, or keep as one dense framing slide?
2. **Slides 02 and 03 both carry a Redox/Asterinas table**, now back to back, on
   different axes (capability, then AI policy). Back-to-back makes the repetition
   more visible — merge into one table with an AI-policy row, or keep them separate
   so each lands its own point?
3. **Slides 05–07** each name one real example. Enough, or does one need a live
   artifact (a probe's output, an A/B diff)?
4. **Slide 08** is the densest of the rest. Split into "docs as cognition" and
   "multi-model / curating context", or leave as one?
5. With the deletions slide gone, the tagline is asserted on slide 01 and only paid
   off in the closing. Does it need one piece of evidence somewhere, or does the
   workflow carry it?
6. **Get the conference-talk video** and either source the verbatim
   formal-verification line or drop the note about it.

## Background

- [`DEVELOPMENT_PRACTICES_REVIEW_AND_ASSESSMENT.md`](DEVELOPMENT_PRACTICES_REVIEW_AND_ASSESSMENT.md)
  — §3 docs as compensating structure, §4 the dev cycle (slides 05–08).
- **Worked examples for slide 06, if one is ever wanted on the slide** (all cut —
  the slide makes the general case instead):
  [`EXT2_FIRST_DATA_BLOCK_FIX.md`](EXT2_FIRST_DATA_BLOCK_FIX.md) — an off-by-one
  that hid because it was symmetric, so only files over ~268 KB exposed it;
  [`MAPPED_PAGE_PREMATURE_FREE_FIX.md`](MAPPED_PAGE_PREMATURE_FREE_FIX.md) — a
  free-after-use that named its own cause (`0xFEEDFACE` poison read back as file
  content), with the `[PMM-RESURRECT]` tripwire still armed and the fix graded by
  rate (6/10 red → 10/10 green);
  [`LONG_ROAD_TO_REDIS.md`](LONG_ROAD_TO_REDIS.md) ·
  [`REDIS_END_TO_END.md`](REDIS_END_TO_END.md) — two theories discredited
  (copy-on-write, `madvise`) before the real cause.
- [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) §18.4 — slide 05's 88.8% → 23.0%.
- [`GOLANG_MISSING_SYSCALLS.md`](GOLANG_MISSING_SYSCALLS.md) — slide 11 (`go build`).
- [`LINE_COUNT_ANALYSIS.md`](LINE_COUNT_ANALYSIS.md) — slide 02's peer group, and
  everything the deck deliberately leaves out.
- **Cut, but worth keeping findable:** every `userspace/` component deleted and
  later restored — `apk-tools` (back in 1 day), `scratch` (9 days), `paws`
  (165 days, and restored 13 minutes before the in-kernel SSH removal). Three
  deletions, three restorations, zero rewrites. Derived from `git log
  --diff-filter=AD` over `userspace/`; was slide 02 through draft 7.
