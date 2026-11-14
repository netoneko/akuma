# Lesson 4 — adjusting expectations, building the loop (living)

**Motivating archive doc:** the plan §Part I lesson 4 (QEMU boots in seconds, `-Xlog:all=info`,
`console.log`, `trace-err`, one probe per hypothesis; case reading:
`BUILDING_KERNEL_WITH_LOCAL_AI_PAIN_AND_SUFFERING.md`). This page is the living version.

## Goal

One sentence: the loop — hypothesis → probe → verdict, throwaway until proven, diagnostics
before bugs — runs in minutes, and the minutes are *measured*, not remembered.

## You have

- The rig: Akuma KVM (not QEMU — plan-wrong wording for this box; same "boots in seconds"
  truth), `vm-probe.sh`/`probe-method.sh` shape, images built by `probe-image.sh` (~60 s).
- The RED suite proven (lesson 2), so a hypothesis about a syscall has a probe waiting for it.

## Do (as-happened, 2026-11-13)

1. **State the hypothesis with a timestamp, inside the capture.** `probe-method.sh` prints
   `HYPOTHESIS H1 (<epoch>): …` before the run that judges it and `CYCLE PASS H1 seconds=N`
   after. The measurement is in the gate log, not in the session's memory.
2. **Measure the probe cycle, not the rebuild cycle — and say which.** Phase 1's cycle =
   hypothesis → verdict: ~1 s probe-run, 9 s whole-method wall clock (boot included). The
   *kernel-change* cycle (edit → build → boot → verdict) is phase 2's first measurement —
   phase 1 must not claim it. — *Plan-wrong:* the plan's done-when says "a hypothesis-to-verdict
   cycle measured under an hour" without saying *which* cycle; a probe-cycle number could have
   masqueraded as the loop's number. Lesson: name the cycle.
3. **Trust the clock, then check it.** Run 2's `H1 seconds=-1` — the host clock slewed backward
   between two `date +%s` reads (machine fact, Akuma 0.0.9). The script clamps and logs both
   epochs (`clock-slewed epochs=…`); a measurement that can silently go negative is a
   measurement that lies once something *depends* on it.
4. **Diagnostics before fixes, always.** Run 1's findings were all probes reading better than
   any fix would have been: `mmap-RWX-allowed` on metal (baseline moved), `AT_SYSINFO_EHDR
   missing` (the vDSO suspect named), `CYCLES PASS 4189 cyc/call` (the VM-trap tax quantified).
   No kernel code was written this phase — the requirements table is the deliverable.

## Gate

| passes when | probe |
|---|---|
| `CYCLE PASS H1 seconds=N` (N < 3600) in the captured log, hypothesis + verdict both inside the capture | `denver/gates/PHASE-1-METHOD.log` |

## Record

- 2026-11-13 (PHASE-1-METHOD GREEN): H1 measured at 0–1 s (clamped; epochs logged), whole
  method 9 s wall clock, three gate runs total (two parser-bug verdicts, one GREEN — the stall
  protocol never reached two hypotheses on one question). Machine facts recorded: host clock
  slew, kernel 0.0.9's RWX baseline vs the guest stack's EACCES, no xattr syscalls (mke2fs
  needed `-E no_copy_xattrs`).

## Record

- 2026-11-13 (PHASE-1-METHOD GREEN): H1 measured at 0–1 s (clamped; epochs logged), whole
  method 9 s wall clock, three gate runs total (two parser-bug verdicts, one GREEN — the stall
  protocol never reached two hypotheses on one question). Machine facts recorded: host clock
  slew, kernel 0.0.9's RWX baseline vs the guest stack's EACCES, no xattr syscalls (mke2fs
  needed `-E no_copy_xattrs`).
- 2026-11-13 (PHASE-2-WX GREEN): the kernel-change cycle, phase 2's first measurement (Do step
  2, kept honest by naming *which* cycle): feature build 24 s cold-chain; rebuild → stage →
  boot → RESULT PASS ≈ 4 min; WIP commit → gate capture ≈ 11 min; one gate run, GREEN first
  capture (the phase-1 RED suite is what bought that — the probes were already proven). The
  per-step number is reconstructed from build outputs, not stamped — L2 debt (1)/(2); the
  diagnostics-first rule paid for itself twice in one session: the console census re-ranked the
  curriculum for free (no rebuild needed to learn `getcpu` was unreached), and the two appended
  boots named the movement raw-SIGSEGV/no-report before any fix could be guessed at.
- 2026-11-13 (PHASE-2-VDSO GREEN): the kernel-change cycle, second measurement, this time
  stamped (the FAULTNAME debt fully paid): edit→build ≈ 20–23 s with two features; boot→ssh
  round trip ≈ 3–5 s (new floor, WX/FAULTNAME was ≈ 8 s); full gate run ≈ 7 s; research →
  GREEN ≈ 80 min including one RED design verdict, two bisect boots, and a probe-constant
  fix. The RED bought the design outright: the bisects (one variable per boot, verdicts in
  the research artifact's table) split map vs auxv-pair in two boots where three hypotheses
  of armchair analysis had argued — the stall protocol's cheapest possible resolution.
