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
