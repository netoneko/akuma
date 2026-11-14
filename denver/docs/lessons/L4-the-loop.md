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
5. **Preserve the failed attempt's capture before re-running (`cp` the gate log first).**
   An honest FAIL is data: PHASE-2-HEAPRO's attempt-1 log held the ring that rewrote the
   root cause (D48) after the fix-in-hand was already proven innocent. A verdict that
   overwrites its own evidence can only be repeated, not learned from.
6. **A probe arm must be able to PASS in principle on the thing it judges.** The rwx arm
   read `/proc/self/maps` — which does not show the mmap slab — and REDd regardless of the
   kernel; three gates carried the "divergence" it named. Before recording a kernel
   divergence from a probe, ask what the probe would print on a kernel that does the right
   thing (D42's rule, extended: a probe *channel* that cannot see the thing reports
   blindness, not verdicts).

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
- 2026-11-14 (PHASE-2-PROCFS GREEN): the kernel-change cycle, third measurement, stamped in
  the capture: complete probe run 188 s (boot ≈ 3 s; the tail is the NAME arm's ssh
  starvation, measured again standalone — see L2); edit→gate-build ≈ 1 min
  (`cargo build -p denver --target x86_64-unknown-none --release
  --features wx-relaxed,vsyscall,procfs-diag`; default build re-verified 30 s); build→stage→
  PASS ≈ 3 min. Host-side checks on this box need the driver's own PATH
  (`PATH=/usr/local/rust/bin:$PATH` — the system Alpine rustc 1.96.1 otherwise shadows the
  nightly) and `--target x86_64-unknown-linux-musl` for host-testable crates (`akuma-procfs`:
  45 tests pass; `akuma-exec`/`akuma-mmu` are bare-metal-build-only). Third execution of the
  D31 probe was the first complete one and it was GREEN — the two-session gate (research +
  probe in session 1, fix + records in session 2) spent its first two runs buying two real
  findings (the false-FAIL constant, the model divergence) before the pass.
- 2026-11-14 (PHASE-2-NULLSTORE GREEN, across a cut): the loop's first **records-after-cutoff
  reconstruction**, and the digest did its designed job. Session 10 reached `RESULT PASS`
  (guest = HEAD, wall 43 s) and was killed before its records; session 11 rebuilt the
  narrative from STATE.md + the gate log + `git diff` alone — no whole-file read, no probe
  re-run needed (the log's guest stamp being HEAD plus its NAME lines matching the
  working-tree probe's format made the evidence current *by construction*). What the
  cutoff cost is now a number: 194k final ctx, 184 calls, 76 min (COST_LEDGER) spent to
  leave records the next session had to rediscover; the records-before-turn-~120 rule is
  load-bearing, not ceremony. Second lesson in the same event: **the rig is a cheap
  oracle after the verdict, too** — the prior session had compiled and staged a canary
  arm (`regpreserve`) but never dispatched it; booting the same gate build once and
  running the java-free arm over ssh converted the gate's "named" verdict into a
  *measured mechanism* (D38) for the price of one boot. Named ≠ understood; if the
  evidence leaves a mechanical question open and a java-free arm can answer it, spend the
  boot before writing the transition.
- 2026-11-14 (PHASE-2-HEAPRO GREEN on attempt 3): the kernel-change cycle, fourth
  measurement — research → GREEN ≈ 70 min, three gate runs (attempt-1 honest FAIL, a
  probe-gap FAIL that was really a PASS of the mechanism, the GREEN), two kernel-change
  cycles (sweep ≈ 2 min build+stage; write-bit ≈ 30 s) and two image re-injections
  (~1–2 min each). The FAIL was the gate's most valuable event: the preserved attempt log
  + the rig's vmrun.log ring rewrote the root cause (the sweep-in-hand was proven innocent
  and the RWX silent W-drop named) at the price of one boot (Do 5). The lying-probe catch
  (Do 6) came free with the rewritten arm: `MMARMS PASS rwx call=42` both tests the fix
  and retires a three-gate "divergence". Movement without a capturable death is a new
  shape: the next gate opens on a capture problem (longer window / output recovery), not
  a fix — and the host-test target fact re-paid (musl target or E0463; L4's PROCFS entry).
