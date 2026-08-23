# AI will write 600 bugs and I will write 600 more — presentation script

**Kirill Maksimov · 2026-08-17**

**Status:** draft 20. The *content* of a talk plus speaker notes.

**Rendered deck:** [`bootstrap/public/600-bugs/index.html`](../../bootstrap/public/600-bugs/index.html)
— 16 slides, keyboard-navigable (↑/↓, Home/End), self-contained apart from
`lock_in.jpg` and `slop_factory.jpg` beside it. Served at `/600-bugs/`. Served from
`bootstrap/public/`, so it is reachable from the VM's own httpd.

> **The HTML is the source of truth, as of draft 20.** Earlier drafts said the
> reverse and the two drifted anyway: draft 19's markdown was missing slide 15
> ("Clean up after yourself") entirely, which had been in the rendered deck for two
> drafts. Edit the HTML, then mirror back into this file. Speaker notes live only
> here — the HTML has no place for them.

**Subject: the workflow that actually built this thing**, and what AI-assisted
development looks like in practice.

Deliberately **not** a numbers talk. Drafts 1–6 were organised around ten
"lessons" derived from line counts and bug densities; that framing is dropped —
the statistics live in [`LINE_COUNT_ANALYSIS.md`](LINE_COUNT_ANALYSIS.md) where
they belong, and the deck now shows the loop that produced them. Slide 06 is the
one place the ledger is shown, and it closes Act I rather than opening it.

**Slides carry headlines and evidence, not paragraphs.** If a slide below runs
past ~8 lines of content it is still too long. The prose under each slide is
speaker material, not slide text.

### Changes from draft 19

Occasioned by six days of work (2026-08-17 → 2026-08-23) that moved enough
capability to invalidate the deck's own numbers.

| Change | Reason |
|---|---|
| **Every statistic re-measured.** Production lines 39,885 → **43,700**; test lines 27,845 → **29,302** (0.70x → 0.67x); documented fixes 622 → **680** (580 kernel-attributable, was 535); syscalls ~140 → **~170**; commits 1,547 → **2,009**; investigation docs 196 → **340** | Requested. The whole Stat 7 bugs/kLoC join was re-derived in [`LINE_COUNT_ANALYSIS.md`](LINE_COUNT_ANALYSIS.md) against the new line areas and the new bug ledger |
| **Slide 03 renamed "What is Akuma?"** and reduced to capability only — the line/fix figures and the per-subsystem table moved off it | Per review. It was doing two jobs under a title that promised both and landed neither |
| **New slide 06, "the bugs"** — the per-subsystem fix table, now closing Act I | Per review. The ledger is a conclusion to the landscape act, not an introduction to it |
| **Slide 04 reframed**: a `runs on` row added, Asterinas' Alibaba Cloud deployment recorded, and the closing note changed from "they publish throughput numbers and I have none" to the case that measuring against Linux and against them is good practice and a source of direction | Per review. The old caveat was obsolete the moment nginx was benchmarked; the replacement is the reason to keep the comparison rather than a verdict on it |
| **Slide 05's Akuma row de-emphasised** — no highlight, and the cell reads just "No policy" | Per review |
| **"0 retries needed" dropped** from the self-hosting figures | Per review |
| **Slide 15 "Clean up after yourself" added to this file** | It has been in the rendered deck since draft 18 and was never mirrored here — the drift that made the HTML the source of truth |
| **nginx added to slide 07's "run X" table**, and two new commit messages to `mood` | New material |
| Slides renumbered throughout: old 06–14 → 07–11, 12–13, 15–16 | Consequence of inserting slide 06. **The back-matter's prose cross-references were already stale before this change** (they point at draft-7 numbering) and are left alone |
| **A benchmarks / Firecracker slide was drafted and cut** | Per review. The Graviton2 port survives as one line on slide 03; the nginx-vs-Docker and llama.cpp numbers are **not in the deck** and live in [`NGINX_MISSING_SYSCALLS.md`](NGINX_MISSING_SYSCALLS.md), [`AKUMA_NET_ISSUES.md`](AKUMA_NET_ISSUES.md) and [`CROSS_CORE_THREAD_COLLAPSE.md`](CROSS_CORE_THREAD_COLLAPSE.md) |

---

## Thesis

> **The problem with writing throwaway software is that people don't actually
> throw away software.**

Sixteen slides. The opener states the trade, Act I places the project and closes
on its own bug ledger, Act II is the workflow and what it produced, Act III is how
it's built.

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

**680 documented, dated, fixed.**

More code generated means more code to check. **But the same shift that made the code
cheap makes the checks cheap** — probes, tripwires, self-tests, boot suites, A/B runs
on real workloads, all cheap enough to write per-bug and leave in place.

**So the checks stop being a budget item you ration, and start being the default
response to every change.**

*Speaker:* that's the whole trade. Not "can it write a kernel" — it can — but that the
volume it produces has to be met with a matching volume of checking, and fortunately
that got cheap at the same time and for the same reason. The workflow act is what I
actually spend the checking budget on.

The number moved from 622 to 680 in six days, which is worth saying out loud if
anyone asks: that is not a bad week, it is a week where two new subsystems
(Firecracker, the NIC path) got the same treatment everything else got.

---

## Act I — The landscape

### 03 — What is Akuma?

**Bare-metal AArch64 kernel in Rust**, `no_std`: preemptive scheduling with
shared-kernel SMP, per-process address spaces with demand paging and CoW fork,
dynamic libraries, ext2, `smoltcp` networking stack, **limited Linux binary
compatibility**, runs Docker images like `redis:alpine`. Comes with `apk` and
Go 1.26.3, Rust and Cargo (stable and nightly), clang / gcc / tcc, git, Bun.

**Runs on QEMU and AWS**, verified on `m6g.metal` — Graviton2, 64 cores — under
Firecracker v1.16.1 on KVM, answering SSH from across the internet.

**A developer oriented operating system inspired by early 2000s experience, with
modern tooling. The first program written for Akuma *inside Akuma* was written by
GLM-4.7 on 22 Aug 2026.**

| | |
|---|---|
| prod lines | **43,700** |
| test lines | **29,302** (0.67x) |
| linux syscalls | **~170** |
| documented fixes | **680** |

The project mark (`src/akuma_40.txt`) and a link to
[github.com/netoneko/akuma](https://github.com/netoneko/akuma) sit in the right
column.

*Speaker:* the deck compares itself to Redox and Asterinas on the next slide, so
this one says what the thing actually is first. **Capability only — the bug ledger
is slide 06 and it closes this act.** The capability table that sat here through
draft 20's first cut was cut again: it restated the prose above it.

The GLM-4.7 line is the one to let land. It is the point at which the thing stopped
being only a target and started being a place you can work — a program written for
Akuma, from inside Akuma, by a model. The platform line is the one that changed
most recently and it is worth pausing on: until 2026-08-21 every claim in this deck
was a claim about QEMU.

---

### 04 — Competitive landscape

| | **Redox** | **Asterinas** | **Akuma** |
|---|---|---|---|
| since | 2015 | ~2022 | 2026 |
| people | team + nonprofit | 50+, 3 universities | **1** |
| funding | EU NGI grants | Ant Group + Intel | none |
| high-water | COSMIC desktop, packages | nginx **faster than Linux**, Firefox, Redis at parity | nginx, Redis + official image, Go, rustc, llama.cpp |
| runs on | real hardware | QEMU, cloud VMs — **in production at Alibaba Cloud** | QEMU · **Graviton2 metal, Firecracker/KVM** |
| builds itself | not yet | undocumented | **yes** |

**Asterinas is the one worth studying.** A *framekernel*: all `unsafe` is confined
to one library (OSTD, ~15k lines, **14% of the kernel** — about the size of seL4's
verified core), so every service above it is written in **100% safe Rust.** They
verify that tiny TCB for memory safety, then reach for **model checking and tests**
for everything past it. Logic-level verification is explicitly aspirational.

**Measuring against Linux and against them is good practice** — a better reference
turns "this feels slow" into something to go fix. It is where a lot of this work
came from.

*Speaker:* this slide sets up Act II. The team most invested in formal
verification, with the funding and the university muscle to do it, scoped proofs to
the 14% where proofs actually work and used practical tools — model checking, real
workloads, tests — for everything else. That's the argument for the workflow: I have
no formal verification and no proofs, so the practical half is all there is, and it
turns out that's what the serious people spend most of their effort on too.

Three honest notes: Asterinas is bigger, faster, better funded and more rigorously
verified than this project **and hosts no compiler** — capability is not one axis.
They are **deployed in production at Alibaba Cloud** (see Sourcing). And where they
overlap me on Redis, they publish throughput numbers.

**Measuring against Linux and against Asterinas is good practice, and it is where a
lot of the work came from.** Having a reference that is unambiguously better is what
turns a vague "this feels slow" into a specific thing to go fix, and most of the
performance work on this project started by putting a Docker container next to the
VM and running the same binary in both. Asterinas plays the same role one level up:
they published nginx throughput against Linux, so nginx became a thing worth running
here. The comparison is a source of direction and inspiration — that is the reason
to keep making it, and it does not need to resolve into a verdict about who is
ahead.

**Sourcing:** the safe-Rust and 14%-TCB claims are from their USENIX ATC'25
abstract and their own blog; the verification-boundary framing is from the blog and
LWN's coverage. A stronger verbatim version of the "formal verification says
nothing about implementation correctness" line may exist in their conference talk —
**unverified, do not quote it as theirs** until the video is checked.

**Asterinas runs in production at Alibaba Cloud.** Stated by the presenters in the
USENIX ATC'25 talk; recorded here 2026-08-23 on Kirill Maksimov's recall of the
video, and consistent with the project's backing (Ant Group). **Provenance grade:
recalled from a talk, no timestamp, no written citation yet.** Before it is
presented: find it in the ATC'25 video and record the timestamp here, or replace it
with a written source (the Asterinas blog or an Ant Group/Alibaba Cloud engineering
post). If neither can be found, keep the note in this doc and drop the claim from
the slide.

---

### 05 — Same pressure, opposite answers

| | AI-contribution policy |
|---|---|
| **Redox** | **banned**, Feb 2026, enforced — LLM-labelled contributions closed on sight; bypassing it is a project ban |
| **Asterinas** | **welcomed and automated** — *"AI is welcome, but the human is responsible"*; ships an AI PR-review bot |
| Akuma | no policy |

Same pressure — generation outpacing review capacity. Two structurally opposite
answers, both from funded, multi-institution projects.

**Both policies legislate *who writes the code*. Neither addresses who keeps it.**

*Speaker:* with no second reviewer there's no bottleneck to legislate, which is
why the mechanised parts — lints that deny by default, a pre-commit hook stricter
than most CI — matter more here, not less. They are the review.

The Akuma row is deliberately flat and unhighlighted as of draft 20: it is the
control in the comparison, not the punchline.

---

### 06 — the bugs

*Closes Act I. Figures first, then the table — no prose on the slide.*

| | |
|---|---|
| documented fixes | **680** |
| prod lines | **43,700** |
| test lines | **29,302** |
| investigation docs | **340** |

| subsystem | fixes | % | /kLoC |
|---|---:|---:|---:|
| Syscall / ABI | 128 | 22.1% | 20.5 |
| Memory & VM | 113 | 19.5% | 23.3 |
| SMP & Locking | 86 | 14.8% | 41.3 |
| Scheduler & Process | 76 | 13.1% | 6.9 |
| Networking | 69 | 11.9% | 11.9 |
| Boot & Drivers | 23 | 4.0% | 6.0 |
| Misc / cross-cutting | 22 | 3.8% | 58.7 |
| Containers | 19 | 3.3% | 17.0 |
| VFS & Filesystem | 17 | 2.9% | 4.2 |
| Console & Terminal | 15 | 2.6% | 15.6 |
| Signals & Exceptions | 12 | 2.1% | 3.5 |
| **kernel** | **580** | 100% | 13.3 |
| outside it — apps, toolchain, ssh | 100 | — | — |

*Speaker:* this is the ledger the rest of the talk draws on, and it belongs at the
end of the landscape act rather than the start — the audience needs to know what the
thing is and who else is in the field before a density table means anything. The
slide is figures and table only; everything below is spoken, not shown.

Bug set is the **580 kernel-attributable** fixes of the 680; the other 100 are
userspace apps, toolchain and SSH, none of which has a kernel line-area to join
against.

**Memory and the syscall layer cost ~4× per line** what the filesystem and driver
code did, under every regrouping tested. That multiple is **drifting and must be
quoted with its date**: it was ~6× on 2026-08-17 and is ~4× on 2026-08-23, not
because memory got safer but because the Firecracker port put twelve documented
fixes into Boot & Drivers and lifted that denominator off the floor. The direction
has survived two measurements; the multiple has not.

**Nothing about concurrency survives regrouping.** One file, `exceptions.rs`, swings
SMP & Locking between 10.5 and 41.3 bugs/kLoC depending only on where it is filed.
If challenged on that row, concede it immediately — it is the most interesting thing
in the data. An earlier cut of this analysis concluded concurrency was *the*
high-density area and named two files as the refactoring queue; re-deriving the
grouping reversed it. Full working, including the retraction, in
[`LINE_COUNT_ANALYSIS.md`](LINE_COUNT_ANALYSIS.md) § Stat 7.

**Bug counts measure *found and fixed*, not present.** Every density figure is as
much a measure of attention as of defect — VFS at 4.2 is either genuinely simpler or
simply less examined, and this join cannot tell those apart.

---

### 07 — Have a concrete goal: run X

**Compatibility with X, in order to run X.**

| target | what it forced |
|---|---|
| **redis** | `/proc/<pid>/*`, `MADV_FREE` honesty, TCP connect-state, short-write `writev` |
| **rust / cargo** | `MAP_SHARED` writeback, 128 KB argv, thread-group reaping, `getpriority` |
| **golang** | signals on a foreign signal stack, pidfd, `waitid` parentage, fork/exec |
| **llama.cpp** | lazy mmap, file-page eviction, heap-growth headroom |
| **nginx** | zero new syscalls — then three kernel defects behind *"stops answering after enough traffic"*: a stubbed `shutdown(2)`, listener-backlog exhaustion, socket reclaim |

**A real program is a specification you didn't write and can't argue with.**

*Speaker:* none of those columns is a list I had to invent. The program decides
what "done" means, it fails in a specific place, and it can't be negotiated with.
Pick the program and the roadmap writes itself.

The nginx row is the newest and the best illustration of the point, because the
opening question was wrong. I assumed it would need user-management syscalls —
"it can't drop privileges because there are no users in this system." The answer
was **zero syscalls**; it needed an `/etc/passwd`. Then the interesting part:
"nginx stops answering after enough traffic" turned out to be **three kernel
defects and nothing whatsoever to do with nginx.** A listener that used to die at
80 churned connections now survives 1088. The program found bugs I was not looking
for, in a subsystem I would not have audited.

---

### 08 — Self-hosting: it compiles itself

Cloned from GitHub and built **inside Akuma**, on a nightly musl toolchain:

```
akuma:/tmp$ git clone https://github.com/netoneko/akuma.git && cd /tmp/akuma && cargo build --release
```

`[data]` Measured in-guest 2026-08-16 (fresh boot, `cargo clean`, `-j4 --offline`,
`MEMORY=8192 SMP=4`) as the Tier 5 arm of a trim-the-fat gate:

| | |
|---|---|
| clean builds run | **20** (10 per arm of an A/B, two different kernels) |
| succeeded first try | **20 / 20** — `EXIT=0`, full `deps/` tree, **3.8 MB ELF** |
| wall clock | **107–128 s** per trial, boot + `cargo clean` + `-j4` build |
| memory-corruption tripwires | **all 0** |
| units | **147 / 147** — and the self-built ELF **boots** |

**Almost nothing gets here.** Redox reached *running* rustc and cargo natively in
January 2026 — a decade in, on the third attempt — and does not yet build itself.
Asterinas hasn't published a self-build result; with 210+ syscalls, Firefox and QEMU
running, a compiler almost certainly works there — nobody has claimed the kernel
builds under it.

*Speaker:* the closer does the same thing for `go build`; this is that shape one rung
up, and it lands here because "run X" pointed at the compiler eventually. Every
number above is in-guest, not host. (The "0 retries needed" figure was dropped from
the slide in draft 20 — "20/20 first try" already says it, and the zero was reading
as filler.)

Be precise about the scarcity, because it is easy to overclaim: "hosts a compiler" is
not the rare part — Asterinas runs Firefox and QEMU, so a compiler running there is
near-certain and simply undocumented. The rare part is **the kernel building itself
and the result booting**, which is a claim someone has to make and verify. The stability claim is the interesting half:
twenty consecutive first-try builds — a failed clean build today is a
regression finding, not weather. That is a recent change; most of the self-hosting
doc is written around riding out intermittent rustc SIGSEGVs with a supervisor
script, and that procedure is retired.

Two honest caveats: my route is easier in one specific way, because Linux ABI plus
musl means unmodified rustc binaries where Redox had to port the compiler onto their
own libc; and "it compiles itself" is a narrower claim than Redox's breadth of
desktop, drivers and packages.

---

### 09 — Audit before touching code

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

Draft 20 is itself an instance: re-measuring for this deck moved every headline
figure, and the bugs/kLoC multiple on slide 06 went from ~6× to ~4× without anyone
touching the memory subsystem.

---

### 10 — Isolate theories with probes, then discredit them

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

The NIC audit is the cleanest recent example. Neither existing profiler could answer
"where does a round trip spend its time", so a per-packet recorder went in at the
virtio boundary. It compiles to nothing when off. It is what turned "networking is
slow" into "there is no virtio-net interrupt and every waiter parks until a 3 ms
timer tick."

---

### 11 — Then A/B it on real workloads

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

Two traps this method walked into recently, both worth mentioning if there's time.
**A single-core boot suite does not verify an SMP primitive**: one change scored a
clean 286/0 at `SMP=1` on a kernel that froze at `SMP=4`. And **back-to-back runs
are not independent** — runs 4–5 of an arm scored *half* runs 1–3 with no code
change, purely from run order, because the TCP stack holds `TimeWait` for 10 s. Any
arm measured second would have been condemned.

---

## Act III — How it's built

### 12 — AI-assisted development: build your intuition

**LLMs are stateless, engineers aren't. Good intuition indicates a strong mental
model.**

Documenting the development process and helping the LLM discover the context by
itself is cheap and always pays off.

| tier | job |
|---|---|
| `runbooks/` | do X, expect Y — ends in **Verify** |
| `reference/` | current state only, each page graded **A / B / C** for how far to trust it |
| `archive/` | 340 investigations, verbatim, **never rewritten** — including the wrong theories |
| agent context | the standing rules, so they're never re-litigated |

**Written to reconstitute project-specific intuition in a reader who starts from
zero.**

**The failure mode this guards against**, logged once in the archive: during what
should have been a mechanical port, two syscalls were **rewritten instead of
copied** — breaking an ABI. Caught and reverted. There is no way to know how many
similar substitutions weren't.

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

### 13 — Learn from history

**A full OS development cycle is a surprisingly large dataset.** 2,009 commits, 340
investigation docs. What falls out of it:

- **680 distinct fixes** across 15 subsystems, itemised, dated, cross-referenced.
- **Two crisis windows, from commit volume alone** — and an independent
  churn-per-file measure lands on the same two months.
- **Recurring defect *shapes*.** ~3% of all fixes are the same underlying bug
  rediscovered in a different subsystem: stale address-space pointer ×3,
  lock-held-across-blocking ×4, readiness gaps ×6 across four files. The
  most-repeated shape: **a raw index outliving the thing it names.**

*Speaker:* the records only became interrogable once they existed in bulk.
Cross-joining lines against bugs turned up a real signal, and also caught me out: my
first cut of that analysis concluded concurrency was the riskiest code per line, and
re-deriving the grouping reversed it. Same data, different filing, opposite answer.

---

### 14 — Clean up after yourself

**Generation is cheap, so the default move is a second copy.** A clone detector over
the kernel found **5,370 duplicated lines** — 5.6% of the tree, and that is the
floor, not the estimate. What the copies cost:

| written more than once | copies | what it cost |
|---|---:|---|
| the CoW break path, `exceptions.rs` | 3 | a refcount underflow in the page-fault path — frames freed while still mapped. Three separate fixes |
| `X` and `X_from_path`, down a whole call chain | 4 pairs | a **second, hand-rolled ELF parser** — which one validates your dynamic linker depends on which `execve` path ran |
| the bounded channel write | 2 | the stream-truncation fix landed on stdout, never on stdin |
| errno tables | 5 | `-12` (`ENOMEM`) returned under a comment reading `ENXIO`. 116 definitions → **39** |
| the runtime-registration pattern | 3 | every network poll took a spinlock to read a function pointer |
| production logic, re-typed inside its own test | 13 | the test exercises its copy, so production drifts for free — plus 25 more that assert `true == true` |

> **"Coding is solved."**
> — some guy with a $2T company

*Speaker:* **this slide was in the rendered deck from draft 18 and was missing from
this file until draft 20** — which is itself the argument for making the HTML the
source of truth.

The last row is the one to dwell on. Thirteen places where production logic was
re-typed inside the test that was supposed to check it: the test passes forever
because it is exercising its own copy. Twenty-five more assert `true == true`. That
is not a testing gap you find by measuring coverage.

---

### 15 — mood

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
2026-08-21  lmao lost networking but how
2026-08-21  hell yeah graviton
```

**Time honored Eastern European tradition: curse, complain, then complain some
more, still do the job.**

*Speaker:* not a joke reel — read in order it's a legible progression. "This is
getting out of hand" recurs four times across two months before it graduates to
"still out of hand"; the two `at least it` lines eleven days apart are the whole of
the first era; and the 2026-08 lines are from the period where every claim carries a
measurement. The tone survived, the rigour arrived. None of it was written for an
audience, which is exactly why it's usable evidence now.

The last two are from the Firecracker session and land eleven hours apart, in that
order. That is the whole method compressed into two commit messages.

*(`mood` is itself one of the commit messages, from the middle era.)*

---

## Closing

### 16 — `go build` defeated

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
> **LLMs are statistical engines and it is your job to turn the stats in your
> favor.**

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
- [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) §18.4 — slide 07's 88.8% → 23.0%.
- [`../runbooks/selfhost-kernel-build.md`](../runbooks/selfhost-kernel-build.md) ·
  [`AKUMA_SELF_HOSTING.md`](AKUMA_SELF_HOSTING.md) — slide 06's numbers.
- [`GOLANG_MISSING_SYSCALLS.md`](GOLANG_MISSING_SYSCALLS.md) — slide 11 (`go build`).
- [`LINE_COUNT_ANALYSIS.md`](LINE_COUNT_ANALYSIS.md) — slide 02's peer group, and
  everything the deck deliberately leaves out.
- **Cut, but worth keeping findable:** every `userspace/` component deleted and
  later restored — `apk-tools` (back in 1 day), `scratch` (9 days), `paws`
  (165 days, and restored 13 minutes before the in-kernel SSH removal). Three
  deletions, three restorations, zero rewrites. Derived from `git log
  --diff-filter=AD` over `userspace/`; was slide 02 through draft 7.
