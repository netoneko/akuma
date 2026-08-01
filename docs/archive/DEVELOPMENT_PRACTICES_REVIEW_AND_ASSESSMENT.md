# Development practices review and assessment (2026-08-01)

**What this is.** A point-in-time outside read of how Akuma has actually been built,
based on `git log` across `main` (1,547 commits, 2025-11-27 → 2026-07-19), the
`docs/` tree (runbooks/reference/archive/userspace), `docs/archive/BUG_FIX_LIST.md`
(the itemized fix audit), and branch/tag history. Written by an AI assistant at the
requesting user's prompt, as a professional-engineer-style assessment of process,
not of any single subsystem. Like everything else in `archive/`, this is a
snapshot opinion, not a living reference — re-derive it rather than trust it once
the project has moved meaningfully past 2026-08-01.

---

## 1. Scale and timeline

| Fact | Value |
|---|---|
| Commits on `main` | 1,547 |
| Span | 2025-11-27 → 2026-07-19 (~8 months) |
| Authorship | 1,531 commits Kirill Maksimov; 8 "Welxoer"; 7 "skad0"; 1 "Khvatov Dmitry" — >99% solo |
| Branches (local+remote) | 316 |
| Version tags | 10: `v0.0.1` (Jan 27) → `v0.0.7-smp-checkpoint` (Jul 24) — real but irregular cadence |
| Lines of Rust today | ~66K `src/` (kernel) + ~37K `crates/` (extracted, host-testable) |
| Confirmed bug fixes logged | ~427 itemized bullets in `BUG_FIX_LIST.md`, plus several inline-batched groups (44 Go syscalls, 20 Bun syscalls, 16 XBPS syscalls, …) — true atomic count comfortably **~490–520** |

Commits per month tell the same story the docs' own churn-derived stability
grades tell independently:

```
2025-11   26        2026-04   55
2025-12   65        2026-05  107
2026-01  268        2026-06  273  <- second fire window (memory + signal crisis)
2026-02  384        2026-07   75
2026-03  294  <- first fire window (syscall-gap crisis)
```

Feb–Mar and Jun are visibly hotter by commit volume alone, independent of the
git-churn-per-file method `docs/README.md`'s A/B/C grading already uses — two
different measurements converging on the same two crisis windows.

---

## 2. Three visible eras

**Era 1 (Nov 2025–Jan 2026) — "make it exist."** Commit messages: `hmm`, `yo`,
`does not actually detect ram`, `at least it still runs`, `this is getting out of
hand`, `oof`, `aaaaaaaa CAT`. Unstructured solo exploration: boot something,
discover the allocator is broken, discover it again. No process yet — the commit
message is a diary entry.

**Era 2 (Feb–Jun 2026) — "fight one subsystem to the ground, branch it, survive
it."** Branch names turn into incident logs: `add-golang-part-2-hell` →
`-fix-ext2` → `-triple-oof` → `-crashes-are-back` → `-memory-mapping`;
`add-golang-futex-nightmare`; `signal-hell-welcome-back`; `qemu-hvf-hellscape`.
Commit messages (`some wild theories from gemini`, `mood`, `maybe there is
progress, maybe not`, `ugh --no-verify`) show rapid, low-ceremony commits during
active debugging, and visible **multi-model orchestration** — both `CLAUDE.md`
and `GEMINI.md` exist as parallel agent-context files, and the log directly
references consulting a second model mid-crisis for alternative theories.

**Era 3 (Jul–Aug 2026) — "campaign discipline."** Numbered phases, audits before
code changes, A/B verification playbooks, stability-graded reference docs, an
archive that's never rewritten. Commit messages flatten into repeated rollups
(`more smp work, bugfixes and docs` ×5). This didn't arrive by policy memo — it
hardened in direct response to Era 2's cost (see §5): the same bug class
recurring, sometimes with the sign flipped, forced the project to start writing
down "correctness rules learned the hard way" so they'd stop being relearned.

---

## 3. The documentation system as compensating structure

Four tiers — `runbooks/` (action-first, ends in **Verify**), `reference/`
(current-state only, A/B/C stability grade from git churn), `archive/` (200+
historical docs, **never rewritten**, linked via "Background" footers),
`userspace/` (pointers co-located with source). This is not decoration. It is a
direct adaptation to a structural fact: the reviewer of this code, on any given
day, is a fresh AI agent (or a human) with no memory of yesterday's session. The
docs are the institutional memory a team would normally carry in heads and in PR
discussion threads.

---

## 4. The dev cycle that emerged (visible in `docs/runbooks/bkl-phase7-workplan.md`)

The current, most mature form of the cycle, observed on the BKL-removal campaign:

1. **Audit before touching code** — re-measure and re-scope, explicitly correcting
   the project's own prior headline numbers when they turn out to be
   instrumentation artifacts (`BKL_PHASE7_AUDIT.md` §1 opens by debunking an
   88.8%-attributed figure down to 23.0%, tracing it to a profiler bug).
2. **Baseline the metric the change is supposed to move**, separate from the
   audit — a throughput number, not just a spin-count proxy.
3. **Execute one narrow sub-phase, then stop for review.** Written directly into
   the prompt: "Start at 7a and stop there for review. Do not attempt the whole
   phase in one session."
4. **A fixed three-part verification bar, every sub-phase:** host unit tests for
   the lock logic → a boot self-test in `src/process_tests.rs` hitting the real
   entry point → a same-binary A/B stress run with zero tolerance for
   stuck/RECOVERED/PANIC/WILD/SPURIOUS/stale-heal signals.
5. **Write it up, update the workplan, add a triage-matrix row** — the next
   phase's prompt reads that doc first.

Supporting habits baked into every prompt, not left to memory: never commit or
push; run clippy across all build configs; no milestone tags in code
identifiers; "never trust a percentage without re-measuring"; purpose-built test
harnesses (`scripts/bkl_smp_regimen/`, mirrored into `scripts/bkl_rustc_bench/`)
carry their own "caveats that cost real debugging time" sections so a gotcha is
paid for once, not per session.

---

## 5. What ~500 bug fixes actually look like

Keyword frequency across `BUG_FIX_LIST.md`'s ~500 entries:

| theme | hits |
|---|---|
| fork/clone/exec | 72 |
| locking | 58 |
| mmap/paging | 46 |
| syscall (general) | 34 |
| epoll/poll | 34 |
| signals (incl. SIGSEGV/SIGPIPE) | 32 |
| wedge/hang/freeze/stuck | 25 |
| OOM | 20 |
| deadlock | 19 |
| corruption | 19 |
| errno/EBADF/EFAULT/ENOSYS | 16 |
| TLB/TTBR0 | 14 |
| race | 13 |

A textbook kernel-engineering distribution: the process model and the
locking/memory substrate under it account for the bulk of it, exactly where a
from-scratch AArch64 kernel building Linux ABI compatibility would hurt most.

### The pattern worth naming: a handful of defect *shapes* recurred across independent call sites

Roughly 15 of the ~500 fixes (~3%) are re-discoveries of the same underlying
defect class in a different subsystem, each found and fixed independently:

- **Stale TTBR0, three times** — `clone_thread`, then `fork_process`, then
  `vfork_process` each independently hit "the identical stale-TTBR0 bug"
  (`OPTIONAL_SMOLTCP.md`).
- **Lock held across a blocking operation, at least four times** — pipe
  `SIGPIPE` raised while holding the `PIPES` spinlock
  (`BKL_FINE_GRAINED_LOCKING_PLAN`); the same shape replayed via OOM inside
  `register_process`'s pipe-buffer growth (`BKL_PHASE7E_PROCESS_TABLE_RECLAIM`);
  `sys_poll_input_event` re-acquiring a spinlock already held on the same path
  (`RICH_TERMINAL_INTERFACE_OVER_SSH`); `sys_sendto` called with preemption
  disabled (`SENDTO_PREEMPTION_FIX`).
- **Epoll readiness gaps, at least six times across four files** — missing
  EPOLLIN for listening sockets, `accept4` blocking instead of EAGAIN, missing
  EPOLLHUP, unreset EPOLLET edges, an always-ready Tap fd, and the
  absolute-vs-per-iteration-deadline bug hitting `epoll_pwait`/`ppoll`/`select`
  as three separate fixes for one root cause.
- **Signal mask scoped per-process instead of per-thread** — found once for Go
  (`SIGNAL_HELL`/`GOLANG_IPC`), rediscovered months later for rustc
  (`AKUMA_SELF_HOSTING` §7k.3) as an unrelated-looking "intermittent SIGSEGV."

One entry stands out as a distinct risk, not a knowledge-transfer gap:
`SPLIT_SYSCALLS.md` logs `sys_nanosleep` and `sys_pselect6`/`sys_ppoll` being
**rewritten instead of copied** during what should have been a mechanical port,
breaking the libakuma ABI and fd-readiness logic — caught and reverted, and
named in the doc as exactly that. It is the sharpest concrete evidence in the
archive of the specific failure mode AI-assisted systems work has to guard
against: an agent "helpfully" rewriting code that should have been ported
verbatim. It was caught this once; there is no way to know from the record how
many similar substitutions were not.

---

## 6. Assessment

**Read the recurrence in proportion.** ~15 re-discovered fixes out of ~500 is
3%. The other 97% is distinct, real, mostly one-shot root causes across an
enormous surface — process model, memory, epoll, TLS, ext2, rump, SMP. The
recurring-defect-shape pattern is the analytically interesting finding in this
dataset, not the dominant one, and should not be read as the headline verdict on
the work.

**The recurrence is a normal cost of the approach, not unique evidence of
failure.** Rediscovering that TTBR0 handling is duplicated across
fork/clone/vfork the second and third time is what happens in *any*
from-scratch kernel until enough call sites have been touched for someone to
notice the duplication and unify it — Linux and the BSDs have decades of
"fixed in arch/x86, forgot arm64" commits with full code review already in
place. A solo project without a second reviewer catching this slightly later
than a team would is a real, nameable cost (see below) — not proof the
approach was wasted.

**The real, structural finding: the project's review bandwidth is one person
plus whichever model they're running.** In a team, the second TTBR0 bug is
often caught at review time by someone who remembers reviewing the first
("didn't we just fix this over here?"). Here, that catch depends entirely on
whoever debugs next finding and reading the right archive doc first — which is
precisely the risk `docs/README.md`'s stability-grade/symptom-matrix system was
built to reduce. It is a reasonable compensating structure, but it is a weaker
guarantee than a shared primitive or a type that makes the bug structurally
impossible, and it depends on every future session actually consulting it.

**The project's own response to this is evidence of a maturing process, not a
stuck one.** "Correctness rules learned the hard way" wasn't written
preemptively — it exists because the BKL ticket-accounting bug came back with
the sign flipped months after the first fix, and the response was to catalog
the failure mode so it couldn't recur invisibly again. A project that hit the
same bug shape repeatedly and did nothing about it would deserve real
criticism. One that hit it repeatedly and then built infrastructure
(stability grades, the phase-workplan verification bar, the hard-don'ts list)
specifically to stop hitting it again is doing what a maturing engineering
process is supposed to do.

**Net.** A solo, AI-leveraged systems project produced a working, self-hosting
AArch64 kernel — preemptive SMP, a real Linux syscall surface, in-kernel SSH,
ext2, a JS runtime, a C compiler, and the kernel compiling itself — in about
eight months, at a pace no individual could sustain unassisted, using
documentation discipline to substitute for the code-review layer it
structurally doesn't have. The ~500-entry bug-fix archive is simultaneously
the best evidence of that throughput and the clearest diagnostic of where the
process's remaining weakness sits: a small number of defect classes that a
second reviewer, or a handful of unifying primitives (one canonical
address-space-switch helper, one epoll readiness state machine, one enforced
"never hold this lock across a blocking call" rule), would likely have
converged on in one pass instead of three or four.

---

## 7. Concrete follow-ups worth doing, if anyone wants to act on §5's pattern

- Unify `clone_thread`/`fork_process`/`vfork_process`'s address-space-switch
  logic into one function, since the same TTBR0 bug hit all three
  independently.
- Give `epoll_check_fd_readiness` one canonical per-fd-type readiness
  contract instead of ad hoc per-consumer logic — six independent gaps across
  four files is a strong signal the abstraction is currently implicit, not
  designed.
- Consider a lightweight lint or a guard type that makes "signal raised / OOM
  triggered while holding a spinlock" a compile-time or debug-assert-time
  error, given it has independently cost four separate debugging sessions.

None of these are urgent — they are exactly the kind of unifying refactor that
becomes obvious only after the third instance of a bug, which is where this
project currently sits.

---

## Methodology note

Based on: `git log --oneline`, `git shortlog -sn`, `git branch -a`, `git tag`,
per-month commit counts on `main`; `docs/README.md`, `docs/reference/README.md`,
`docs/runbooks/README.md`, `docs/runbooks/bkl-phase7-workplan.md`,
`docs/reference/subsystems/locking.md`; `docs/archive/BUG_FIX_LIST.md` in full,
plus direct reads of `AI_DEBUGGING.md`, `ARCHITECTURE_QUESTIONS.md`,
`KNOWN_ISSUES.md`, `HIJACK_VS_KERNEL_PROXY.md`, `EMBASSY_REMOVAL.md`,
`DEAD_CODE_ANALYSIS.md`, `TRIM_FAT_PART_1.md`, `BKL_PHASE7_AUDIT.md`; root
`README.md`, `GEMINI.md`, `proposals/CLEANUP.md`, `acceptance/03_two_vms_agent_workflow.md`.
