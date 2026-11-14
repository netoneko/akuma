# Denver

```text
   /\        /\        /\        /\        /\
  /  \  /\  /  \  /\  /  \  /\  /  \          D E N V E R
 /    \/  \/    \/  \/    \/  \/    \        https://akuma.sh/
_/                                    \_     Beans System Debauchery:
  ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~       kernel for Clojure
```

Denver is an experimental platform kernel, written almost entirely by GLM-5.3-flash
working in autoresearch mode — one gate per session, captured probe output as the only
judge (`docs/runbooks/AUTORESEARCH.md`), a human operator in the loop. The goal was
twofold: condense the previous experiments into a set of generic lessons for building
platform kernels, and prove them on a concrete platform — here, the JVM.

It took the loop about 24 hours to go from a bare kernel that boots tokio to enough
artifacts to run Java, Clojure, Scala and Groovy — and to serve an HTTP server written
in Clojure, from a fresh kernel booting under KVM on metal. Every rule below is proven
by a captured gate log or a recorded decision — nothing here is doctrine without
evidence. The links go to the living lesson files, which carry the full worked example
and the per-gate `Record` trail.

## A platform kernel as a collection of crates

Denver is not a kernel written from scratch. It is a manifest: one binary crate and one
front crate that pick, by name, which existing Akuma crates make up the kernel. The whole
`Cargo.toml` of the binary is a list of features, each relayed 1:1 to the front crate and
off by default, so the default build stays what it was (rule R6):

```toml
[package]
name = "denver"

[dependencies]
akuma-jvm-kernel = { path = "../crates/akuma-jvm-kernel", features = ["no-tests"] }

[features]
wx-relaxed       = ["akuma-jvm-kernel/wx-relaxed"]        # JIT code cache: RWX, RW -> R+X
vsyscall         = ["akuma-jvm-kernel/vsyscall"]          # the legacy page HotSpot hardcodes
fold-regpreserve = ["akuma-jvm-kernel/fold-regpreserve"]  # keep the caller's six arg registers
sysinfo          = ["akuma-jvm-kernel/sysinfo"]           # heap sizing reads it
fault-signal     = ["akuma-jvm-kernel/fault-signal"]      # null checks reach HotSpot's SIGSEGV handler
shell            = ["akuma-jvm-kernel/shell"]             # getrlimit, getpgid, ... (full file: Cargo.toml)
```

`akuma-jvm-kernel` ([`crates/akuma-jvm-kernel`](../crates/akuma-jvm-kernel)) has no
capability of its own: it re-exports `akuma-tokio-kernel`'s `entry!` macro and `Kernel`
builder and maps each feature name onto the crate below it. A new capability is one feature
line per layer, never a fork.

```text
 denver                     the binary: main.rs, banner.txt, the feature list
   |   features, 1:1
   v
 akuma-jvm-kernel           front crate: entry!, Kernel, zero capabilities
   |   features, 1:1
   v
 akuma-tokio-kernel         the platform: tokio on bare metal (Taoxichuan's, shared)
   |   picks the machine half by target
   +--> akuma-tokio-kernel-aarch64
   '--> akuma-tokio-kernel-amd64    the machine half: the arms the features switch on
          |
          +-- akuma-runtime-tokio, akuma-syscalls-glue(-amd64), akuma-exec
          +-- akuma-mmu, akuma-pmm, akuma-vfs, akuma-ext2, akuma-net, akuma-tty
          '-- akuma-trap-amd64, akuma-ring3-amd64, akuma-sched-amd64, ...
```

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

## House pointers

- Lessons index (file map, renumber note): [`docs/lessons/README.md`](docs/lessons/README.md)
- Governing runbook (the loop, R1–R9, file ownership): [`docs/runbooks/AUTORESEARCH.md`](docs/runbooks/AUTORESEARCH.md)
- Requirements table the lessons feed: [`docs/REQUIREMENTS.md`](docs/REQUIREMENTS.md)
- The experiment's records and captured evidence: [`RESULT.md`](RESULT.md), [`docs/LOOP_LOG.md`](docs/LOOP_LOG.md), [`gates/`](gates/)
