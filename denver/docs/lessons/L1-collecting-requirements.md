# Lesson 1 — collecting requirements (living)

**Motivating archive doc:** the plan §Part I lesson 1; worked example
`TAOXICHUAN_JVM_ATTEMPT_2026_11_09.md` (**not on branch `denver`** — recovered from git
`d7878378`; a fuller `main` checkout lives at `/src/akuma-merge`). This page is the living
version — *Do* rewritten as-happened, *Record* appended per pass.

## Goal

One sentence: the workload tells you what to build — run it, catalogue every failure as a
capability gap with evidence, and rank by the workload's own order, never by taste.

## You have

- A kernel that boots tokio (`uname` says denver, phase 0 GREEN) and a host box whose kernel is
  also Akuma — two kernels to A/B against, one binary to run on both.
- The workload one rung up the ladder: `java -version` (Corretto 21.0.13 alpine musl x86_64,
  188.8 MiB, already on the host).
- No requirements table yet; the attempt doc's blocker table as the format to beat.

## Do (as-happened, 2026-11-13)

1. **Run the workload on the best kernel first.** `java -version` on the host metal → SIGSEGV
   0.02 s into `JNI_CreateJavaVM`, `hs_err` written. — *Plan-wrong, quoted:* the plan says "run
   the workload on the best kernel" expecting the richest kernel to fail *latest*; on Akuma the
   richest (metal 0.0.9) failed at the vsyscall page **before** the known W^X blocker fired —
   a gap the aarch64 attempt never saw. The metal leg is not a formality: it is where new gaps
   show up first.
2. **A/B the same binary on every kernel you can reach** (D11). Same JRE into the guest image
   (`cp -a` the host's tree — both userlands are alpine musl, zero rebuilding) → guest run.
   The guest died at W^X (the attempt's blocker 1), the metal at vsyscall — two different first
   failures from one binary, and each kernel's gap list starts where its own run stopped.
3. **Order by the workload's init order.** The attempt's order (W^X → getcpu → NULL store)
   reproduced on the guest exactly; REQUIREMENTS.md's rank is that order, with the metal-only
   vDSO gap slotted after the guest-proven ones. — *Plan-wrong:* "suspects stay hypotheses"
   needs a place to *put* them; the table's Suspects section (kept out of the rank) is that
   place — the NULL store is unreachable until W^X closes, so it must not outrank getcpu.
4. **Write the table before any closing code.** `denver/docs/REQUIREMENTS.md`: gap → evidence
   (the captured verdict lines, verbatim) → closing pass. Evidence lines come from the gate
   log only, never from memory.

## Gate

| passes when | probe |
|---|---|
| `denver/docs/REQUIREMENTS.md` exists, every row's evidence is a captured verdict line, rank = workload init order, suspects separated | `denver/gates/PHASE-1-METHOD.log` (guest + metal legs) |

## Record

- 2026-11-13 (PHASE-1-METHOD GREEN): table written from first-hand lines; G1 W^X (guest-proven),
  G2 getcpu (both kernels), G3 vDSO (metal-proven, guest-unreached), G4 procfs auxv/maps
  (diagnostics), G5 time cost (deferred). Decisions: D11 (A/B both kernels). Zero interventions.

## Record

- 2026-11-13 (PHASE-1-METHOD GREEN): table written from first-hand lines; G1 W^X (guest-proven),
  G2 getcpu (both kernels), G3 vDSO (metal-proven, guest-unreached), G4 procfs auxv/maps
  (diagnostics), G5 time cost (deferred). Decisions: D11 (A/B both kernels). Zero interventions.
- 2026-11-13 (PHASE-2-WX GREEN): the rank is *alive* — closing G1 moved the first blocker to an
  unnamed SIGSEGV that fires before `getcpu` (console census: `nr=168` never called). The table
  re-ranked openly with the reason recorded in the file (its own pre-authorized clause: "re-rank
  openly if evidence changes it"); new row G6 + new order FAULTNAME → GETCPU → PROCFS → VDSO.
  Lesson: the rank is a snapshot of the workload's init order *at a kernel policy state* — a
  closed gap re-orders the queue, so every pass ends by re-reading the table against fresh
  evidence, not by taking its own NEXT line forward blindly.
- 2026-11-14 (PHASE-3-JAVAWORKLOAD GREEN): the ladder climbs by **workload reach**, not kernel
  guesswork — the suspects (readlink 89, setitimer 38, membarrier 324, getrlimit 97, prlimit64
  302) re-measured non-fatal under a real class load, so they stay hypotheses; they only become
  rows when a workload that REACHES them dies naming one. The next rung was therefore chosen for
  reach, not for kernel convenience: exceptions, user threads, java.io file I/O, G1 allocation
  pressure (G12). Also measured: there is no javac anywhere in this lineage (both JREs; JEP 330
  dead in VM init; no bytecode precedent in archive/reference/shanghai/taoxichuan) — the
  workload instrument itself had to be built (`mkhello.py`, hand-assembled bytecode).
- 2026-11-14 (PHASE-3-JAVACLASSLIB GREEN): the "no javac anywhere in the lineage" premise was a
  **question to ask, not a reason to build** — the operator's escalation channel (`ESCALATE.md`,
  `QUESTION:`) produced real-javac classes the same day, the hand assembler retired, G12 closed
  with `WORKLOAD-ALL-PASS` on kernel d4acb339 unchanged. Record: a capability table row built on
  a search result ("not in the repo") must not silently become an existence claim ("nowhere").
- 2026-11-14 (PHASE-3-JAVAC GREEN): the operator's priority change re-ranked the table FIRST
  (G13) and the gate closed it without new kernel code — the fault-signal port was session 18's
  work; the proof is one probe with a java-free arm (sigprobe) answering the kernel-path question
  before any JVM run. Ride-along discipline: the same probe measured sigprobe's un-restored
  `%rax`, which became the NEW first-ranked row G14 (PHASE-4-SIGRAX) — every pass ends by
  re-reading the table, and a ride-along datum outranks stale deferrals when it is the only open
  item on its path. Also closed off-kernel: the `shell` syscall set (G15) — resolved by finding
  an existing feature, zero denver kernel code (lesson L5).
