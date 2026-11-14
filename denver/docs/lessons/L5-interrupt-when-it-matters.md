# Lesson 5 — interrupt when it matters (living)

**Motivating archive docs:** `denver/docs/research/PHASE-3-JAVACLASSLIB.md` (the
correction of D64's premise, "a question to ask, not a reason to build", and Decision
D69), L1's JAVACLASSLIB Record (the seed sentence), the runbook's escape hatch
(`AUTORESEARCH.md` § escalate: `denver/ESCALATE.md`, `QUESTION:` / `CONTEXT:`, archived
to `docs/archive/escalations/` once answered), and the operator's night brief
(`scripts/kvm/goose/denver-01-autoresearch.md`). Written 2026-11-14, after the
experiment concluded, from the recorded evidence.

## Goal

One sentence: the first path found is not the path required — humans and AIs alike get
caught up chasing the thing they found first, so before chasing, interrogate whether you
can go around it; a well-aimed interruption (the agent asking a question early, the
human redirecting by intuition) is the cheapest tool in the box.

## You have

- An agent mid-detour on a self-imposed path that is *working*: D64's R9 repo search
  (`docs/archive/`, `docs/reference/`, `shanghai/`, `taoxichuan/`) returned "no javac
  anywhere in the lineage" — correct as a search, wrong as an existence claim — and the
  answer was converted into a build: `mkhello.py`, a hand bytecode assembler whose
  `Hello.class` loaded and printed on kernel `d4acb339` unchanged (PHASE-3-JAVAWORKLOAD
  GREEN, wall 9 s, commit `4574490d`).
- The next rung demanding more of the same instrument: G12 (exceptions, user threads,
  file I/O, G1 pressure) means a bigger class file, so the assembler grows
  (`mkworkload.py`) and grows a structural verifier (`verifyclass.py`) to check itself.
- A human with context no repo search can reach: a real `javac` (Homebrew OpenJDK
  26.0.2, `--release 21` → major 65, compiler-generated StackMapTable) exists on the
  operator's Mac, one question away — and a channel built for exactly this:
  `denver/ESCALATE.md`, `QUESTION:` lines, answered by a human or a stronger model.

## Do (as-happened, 2026-11-14)

1. **The detour was delivering — that is what makes the trap real.** The hand assembler
   was not a failure: it closed the previous rung (JAVAWORKLOAD) first-hand on an
   unchanged kernel. Momentum therefore read as competence, and the next rung was
   answered with more of the same: `mkworkload.py` plus its own verifier. The cost was
   about to scale, and nothing in the loop flagged it, because nothing in the loop asks
   *should this path exist* — only *does this path work*.
2. **A self-verifying toolchain is the tell.** `verifyclass.py` caught two real builder
   bugs pre-boot (ldc operand width; a Code attribute missing its header) and then
   passed a structurally invalid file, because builder and checker had omitted the same
   thing (`interfaces_count`, JVMS 4.1) — the guest's HotSpot named it in <1 s through
   the pipe arm (`ClassFormatError`). Lesson L2's rule ("the receiver is the verifier")
   was paid for by this exact detour. When the tool that judges your build shares the
   build's assumptions, the build is answering its own question, and no amount of
   pre-boot checking will catch it.
3. **The interrupt was a question, not a fix.** The operator's correction, quoted in the
   research artifact: *"that was a question to ask, not a reason to build."* D64's
   premise died on contact with the lineage's unsearched member — the operator. Real
   classes were staged the same day (`/root/denver/javac/`, `Workload.java` /
   `Worker.java`, D69), the first JAVACLASSLIB attempt's `exit=1` turned out to be the
   class files and not the kernel at all, and G12 closed with `WORKLOAD-ALL-PASS` on
   `d4acb339` unchanged (gate wall 17 s). The assembler retired to `denver/probes/` as
   a recorded instrument with known-good output. One question plus files that already
   existed replaced a session of compiler construction.
4. **Interruptions cut both ways, and both directions are data.** Agent-side: escalate
   *before* the build starts — the channel exists precisely so "I couldn't find a
   compiler anywhere, does one exist?" costs minutes (L1's JAVACLASSLIB Record). Human-
   side: the operator's night update reordered the same day's plan (gate first,
   REQUIREMENTS second, paperwork last) so a quota cutoff would lose nothing but polish
   (L4 records-before-polish), and the JAVAC gate's three probe-side interventions were
   exactly the hardening the next probe inherited for free. The shared asymmetry: the
   interrupt costs minutes; the detour it prevents costs a session and ships a subtle
   wrongness.

## Record

- 2026-11-14 (post-experiment, from the evidence): the rule is **not** "never build" —
  the assembler's hours were not wasted (G11 GREEN, L2's receiver-is-verifier rule, a
  recorded instrument with known-good output survive it). The rule is that the first
  path found is not the path required: the detour was work on a false premise, and the
  same hours spent after one `QUESTION:` line would have produced the gate with zero
  assembler code. The interrupt fired when it mattered — at the rung where the cost was
  about to multiply — and the record keeps both sides honest: what the detour bought,
  and what the question bought for the price of asking.

## Checks

    # has the question been asked before the build starts?
    grep -n "QUESTION:" denver/ESCALATE.md        # archived to docs/archive/escalations/ once answered
    # what did the search negative actually cover — the tree, or the world?
    git grep -l -i javac -- docs/archive docs/reference shanghai taoxichuan   # D64's sweep was in-tree only
    # the interrogation, in order, before any instrument build:
    #   1. does it exist outside this tree (human, other machines, sibling repos)?
    #   2. has the human been asked?
    #   3. what does going around cost vs building — and which cost scales next?
