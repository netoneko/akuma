# Lesson 2 — writing probes (living)

**Motivating archive doc:** the plan §Part I lesson 2 (`jvmprobe`, `getcpu`, `getauxval`,
cycles-per-call probes, "in the `mwprobe` style"). This page is the living version.

## Goal

One sentence: every gate is a probe, written before the kernel code that would close it — and
the RED phase proves the probes themselves before any pass claims anything.

## You have

- Probe precedents in-tree: `userspace/jitprobe/c/jit_probe.c` (one binary, named arms, crashes
  read as CRASH, `JITPROBE OK` marker) and `userspace/tokioprobe/` (named `STEP PASS` lines,
  bounded timeouts, `RESULT PASS`, static musl). `mwprobe` itself is on the Mac only — the style
  survives in these two.
- The rig: `denver/vm-probe.sh`'s shape (free slot → boot → ssh round trip → probe → always
  stop) and an image builder path (`mke2fs -d`, the attempt's trick).

## Do (as-happened, 2026-11-13)

1. **One binary, named arms** (`denver/probes/jvmprobe.c`): `getcpu`, `auxv`, `cycles`, `rwx`,
   `java` — each printing `NAME PASS/RED detail`, exit 0 iff PASS. The phase-2 gates grep the
   arm names; that vocabulary is fixed now. Crashy arms run in a forked child (jitprobe's rule).
2. **Build on the box you have.** — *Plan-wrong, quoted:* tokioprobe's `build-musl.sh` needs
   rustup + a musl cross-gcc; this box has neither. But the host userland is **alpine musl**
   itself: `gcc -static` produces static-pie musl ELFs natively. Plain C, 260 LOC, no new
   toolchain (R5 held).
3. **Unbuffered stdio, or the verdicts lie.** Forked-child verdict lines vanish under pipe
   buffering at `_exit` — run 1 lost `RWX PASS rw_to_rx=42` exactly that way (TTY vs pipe:
   line-buffered vs full). `setvbuf(stdout, NULL, _IONBF, 0)` in `main`, `fflush` before any
   computed jump. A probe whose lines depend on how it was captured is a broken probe.
4. **Carry the reason, not just the line.** `java -version` prints the blocker on line 2
   (`Failed to mark memory page as executable…`) — run 1's verdict kept only line 1 and the
   evidence read as "initialization failed", which names nothing. Verdict line = first line +
   reason line + raw output.
5. **The RED run is the gate.** `denver/probe-method.sh`: metal leg (A/B) + guest leg, every arm
   bounded (`timeout` everywhere), platform sanity (`tokioprobe RESULT PASS`) before method
   evidence counts, and the suite's verdict is *clean verdict lines exist*, not *arms passed* —
   RED is the expected phase-1 outcome (gate = the method, phase-0 D12 convention).
6. **The capture carries every side the evidence lives on.** — *Defect, quoted, fixed
   2026-11-13 (PHASE-2-FAULTNAME, Finding 0):* session 4's records said the kernel prints
   `[Fault] slot 5 killed (SIGSEGV)` "without an address" — but its own console census
   (`/root/denver/w/vmrun-diag.log`) carried
   `[Fault] ring3 PageFault rip=0xffffffffff600800 … cr2=0xffffffffff600800 code=0x14` one
   line above. The gate captures never carried the console (ssh-side output only), and the
   census quote dropped the line that mattered. Rule: a gate's capture must include every
   channel the evidence lives on (here: the guest console = `$W/vmrun.log`, read with per-arm
   byte-offset attribution, `-a`-tolerant greps), and a census quote carries the whole fault
   block — not the line that fits the story.

## Gate

| passes when | probe |
|---|---|
| every arm prints a clean `NAME (PASS|RED)` line on both kernels; nothing hangs; the suite ends `RESULT PASS` with RED as the expected verdicts | `denver/gates/PHASE-1-METHOD.log` |

## Record

- 2026-11-13 (PHASE-1-METHOD GREEN): RED suite green first-hand — guest `GETCPU RED ENOSYS`,
  `AUXV RED AT_SYSINFO_EHDR missing`, `RWX RED EACCES` ×2, `CYCLES PASS`, `JVM RED …Failed to
  mark memory page as executable…`; metal leg + hs_err captured. Runs 1–2 failed the *suite's
  own* verdict on parser bugs (lowercase grep, eval quoting) — the stall protocol's
  "one hypothesis → one change → re-run" never needed a third; the probes were proven by being
  trusted over the parser, not the reverse.

## Record

- 2026-11-13 (PHASE-1-METHOD GREEN): one binary, named arms, phase-2 gates grep the names.
- 2026-11-13 (PHASE-2-WX GREEN): the vocabulary paid off first-hand — `RWX PASS rw_to_rx=42`
  judged the gate on the first capture, on the same image that RED'd it in phase 1. Two probe
  debts filed for the next gate's research: (1) the `java` arm names exit-vs-signal but not the
  *fault* — a raw `signal=11 first=""` was past-blocker-1 by the letter and opaque in fact; the
  next gate (FAULTNAME) exists to make such deaths readable (kernel prints rip/cr2). (2) The
  probe scripts carry no epoch stamps, so wall-clock per step is reconstructed, not measured —
  stamp `date +%s` at each arm (probe-method.sh's H1 shape already does this; copy it).
  Rig facts: bare `ls` is not on the guest ssh PATH (`/bin/busybox ls`); `-Xlog:all=info`
  wedges the guest — do not use it as a diagnostic there until the wedge is understood.
- 2026-11-13 (PHASE-2-FAULTNAME GREEN): both debts paid — `denver/probe-faultname.sh` stamps
  every step (`elapsed=`, slew-clamped, both epochs in the capture) and windows the console
  log per arm (byte offsets into `$W/vmrun.log`), so the capture itself names the death:
  `FAULT PASS rip=0xffffffffff600800 cr2=0xffffffffff600800 code=0x14
  module=legacy-vsyscall-page` — the metal `hs_err` pc, now cross-machine confirmed from the
  guest. The Do-6 defect was found and fixed in the same session (research Finding 0, before
  any run). New sibling of the "verdicts lie" rule, parse-time edition: run 1 was already
  RESULT PASS while its module branch compared prefix-less hex against the `0x`-prefixed
  constant and printed a false SURPRISE — one-line fix, re-run, judged on run 2, the
  overwrite of run 1 recorded in RESULT/LOOP_LOG rather than hidden. A probe that has passed
  can still be lying; check its *interpretation* lines, not just its verdict.
- 2026-11-13 (PHASE-2-VDSO GREEN): the same rule, third limb — the *constant*. Run 2 was
  already RESULT PASS while its expected-value constant for the auxv arm disagreed with the
  kernel's correct disclosure (`AT_SYSINFO_EHDR=18446744073699061760` = `0xffffffffff5ff000`,
  python-verified; the hand-computed decimal in the probe was wrong) and printed a false
  SURPRISE. Rule upgraded: **every ABI constant that lands in a grep pattern is data —
  derive it with a tool (`python3`/`printf '%d'`), never by hand**, and prefer grepping the
  hex with the value converted once, where a wrong digit is visible. The FAULTNAME debt
  paid again in the same session: the RED-2 console window named the bisect verdict's death
  (`cr2=0xffffffffff600020`) first-hand — capture-with-attribution did the research work of
  a whole diagnostic pass.
- 2026-11-14 (PHASE-2-PROCFS GREEN, after one false FAIL): the same rule, fourth limb — the
  *variable*, not the value. Run 1 FAILed a healthy rig because the probe defined `VDSoHASH`
  but compared `$MERGEDHASH`; the unset variable expanded to the empty pattern, and `*""*`
  matches any uname — the python-checked-constants rule guards the *value*, nothing guarded
  the *name*. Fixed by naming, plus a fail-fast guard (an empty identity constant aborts the
  probe before boot), so the failure mode can only be loud, never spurious. Same session, a
  probe-cost fact measured twice: the guest's sshd cannot carry a connection while the NAME
  arm runs java with the 2 ms maps poll (nine reconnect stalls over ~2.5 min, no recovery
  even after the workload died) — a probe's own cadence is part of its cost; the attribution
  report may need a slower poll or a console-side path (G9's session owns that choice).
