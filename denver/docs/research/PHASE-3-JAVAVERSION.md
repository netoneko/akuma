# PHASE-3-JAVAVERSION — the capture gate: make `java -version`'s fate a readable datum

Session 16, 2026-11-14. Phase 2 is exhausted (G1/G6/G7/G9/G8/G4/G3/G2 GREEN, G5 deferred);
phase 3 opens with the capture problem the HEAPRO→MEMINFO→GETCPU windows left standing:
java's fate has been "no fault, no exit, output unrecoverable over ssh" for three
consecutive gates. This gate's deliverable is a READABLE datum, not a java-side fix
(diagnostics-before-bugs, the runbook rule applied a fourth time).

## The confound (D59) — the starvation datum was never java-only

Every java window so far dispatched the workload through the `name` arm
(`jvmprobe name /opt/jdk/bin/java <rip> -version > /tmp/java-out.log 2>&1`;
probe-heapro.sh:195, probe-nullfix.sh:178, probe-meminfo.sh:210, probe-getcpu.sh).
`arm_name` (denver/probes/jvmprobe.c) runs java in a forked child and **its parent polls
`/proc/<pid>/maps` every 2 ms** (500 Hz open+read syscall pairs) until the child dies —
bounded only at 15000 polls / 30 s, after which `waitpid` blocks indefinitely. So every
"sshd starved, output unrecoverable" observation was made with a live 2 ms poller (plus
`trace-err` console prints for any failing open) running on the same 4-vCPU guest. The
recorded phrase "java runs + polls at 2 ms" (REQUIREMENTS re-rank) conflated the probe's
poller with the workload. **H1: the poller, not java, killed the recovery channel.**

## Hypotheses this gate separates

- **H1 (channel):** poller-less dispatch restores ssh; the datum reads over ssh.
- **H2 (bridge):** the exec bridge serves one session at a time; any second connection
  while a foreground command runs goes unanswered — nothing to do with java.
- **H3 (workload):** java itself spins/blocks without yielding and starves sshd
  (scheduler fairness); the channel dies whenever java lives, poller or not.
- **H4 (exit-unobserved):** java exits quickly but every prior window missed it (weak —
  the 180 s timeout fired three times while the `waitpid` was still alive, so java had
  not exited by then; the longer foreground window settles it anyway).

## Design (no kernel code; the gate runs the proven GREEN rig unchanged)

The instrument is a new probe, `denver/probe-javaversion.sh`, shaped on probe-getcpu.sh
(stamps, free-the-slot first, stop at the end — L2's rules; the console window taken with
a per-arm byte offset, L2's "the channel the evidence lives on is the guest console =
`$W/vmrun.log`").

- **Rig:** reuse `/root/denver/w/denver-getcpu` + `vm-getcpu.spec` — the d4acb339 binary
  every phase-2 gate proved. Capture gate ⇒ no code moves ⇒ the R6-perfect gate: uname
  identity REQUIRES `d4acb339` and REJECTS every older rig hash. Any kernel change would
  be a guess before the datum reads.
- **Rides (sanity, cheap):** tokioprobe RESULT PASS; `jvmprobe getcpu` PASS (cpu<4,
  node=0) — proves the right binary AND feature set booted (getcpu/sysinfo/wx/vsyscall/
  procfs-diag/fold/lazyrel all ride in that one PASS). regpreserve/lazyrel/mmaparms are
  NOT re-run: nothing changed since their GREEN, and the capture gate's wall clock is
  better spent on the window.
- **B0 (channel baseline):** poke `echo ALIVE` before any dispatch — the channel works.
- **B1 (concurrency baseline):** a background `sleep 45` session while a second
  connection pokes `uname` — answered ⇒ concurrent sessions work ⇒ H2 dead. Requires
  `sleep` in the image (Alpine JDK carries it; verified guest-side, skipped-with-datum if
  absent).
- **THE ARM:** java runs FOREGROUND on its own session, host-side backgrounded, output
  guest-side to `/tmp/java-out.log` plus an `echo JAVARC=$?` exit marker appended by the
  same shell line. Window 300 s (LONGER window — 10x the old 30 s), then pokes every
  45 s DURING the run (each poke records ANSWERED/UNANSWERED — the poller-less
  starvation datum — and grabs partial output) and two recovery tries after.
- **Console window:** from the byte offset taken at dispatch — fault lines (old rip
  `60000067cd7a` must stay absent), kill lines, and a **census**: `[nosys]/[err] nr=…`
  bucket counts (trace-err is ON in this binary, so a hang's blocking syscall shows up
  as repeated nr lines — the next gate's datum regardless of verdict).
- **Console-side probe:** `echo PROBE > /dev/console` — records whether a user→console
  path exists (it is expected NOT to; see D61). If it exists, java's output can go
  straight to the console and the ssh channel stops mattering.
- **The mmdbg hatch stays a recorded suspect (D51), unranked:** if the window's census
  ever points into mm release bodies, the mmdbg-gated prints are the bisect instrument.
  Not activated here — no evidence points there yet.

## Verdict (D60) — PASS means the fate is readable, not that java works

1. **READABLE-EXIT:** the arm returns with a JAVARC line and version output recovered →
   the ladder's first rung is reached; PASS.
2. **READABLE-DEATH:** a fault line names a rip in the window (old rip ⇒ also flag the
   regression, though the same binary cannot regress — staging is the suspect and the
   uname check already caught it); PASS — the death is named, procfs attribution rides.
3. **READABLE-HANG:** no fault, no exit by window end, but the observation channel was
   proven alive (B0/B1, and at least one poke ANSWERED during or after the run) and the
   census + window are dumped → "java hangs indefinitely without dying" is a readable
   datum; PASS.
4. **FAIL** when the capture rig itself could not observe: sanity rides RED, dispatch
   never happened, the window is empty of all evidence, or every poke went unanswered
   WITH B1 proving concurrency works (⇒ H3 with a dead channel ⇒ the next pass is the
   console-side capture of D61, still diagnostics, and this gate re-runs under it).

## Precedents (R9) — what was searched and found

- `rg '"/dev/console"|/dev/console'` across all workspace crates: **no kernel serves a
  /dev/console node** — the path would be new work (D61), not composition.
- The composition precedent if D61 is ever needed: `/dev/tty`'s fd shape
  (`akuma-syscalls-glue/src/fs.rs` ~2189–2256: `alloc_fd(FileDescriptor::DevTty)`,
  read/write arms at fs.rs:389/1222; glue already prints to the console via
  `akuma_primitives::safe_print!` for `SYSCALL_DEBUG_IO_ENABLED` lines) — a
  `DevConsole` variant behind an off-by-default `console-dev` feature is the same shape.
- Kernel→user log precedent only (no user→kernel log anywhere): the metal kernel's
  `syslog(2)`/klogctl arm (`akuma-kernel-amd64/src/usermode/dispatch.rs:1306-1310`,
  `usermode.rs:745`) over the `akuma-dmesg` ring; `akuma-comconsole` is the metal COM
  driver. Neither takes user bytes to the console.
- shanghai/taoxichuan call `.console()` as a kernel capability only — same as denver's
  front-crate `console` feature; no user-side console device precedent exists in any
  kernel of the lineage.
- L2's standing choice ("the NAME arm's report may need a slower poll or a console-side
  path — G9's session owns that choice", lessons/L2-writing-probes.md) is taken HERE:
  slower poll first (zero code); console-side only if the verdict lands in case 4.
- FAULTNAME's lesson (phase-2, G6): the delivery path already names user deaths on the
  console — the window reader, not a new kernel print, is the instrument.

## The one experiment proposal (per-phase rule)

Knob: `DENVER_COMPACT=1` (forwards `GOOSE_AUTO_COMPACT_THRESHOLD`, OFF by default,
untested on GLM per EFFICIENCY.md). Expected effect: the ledger's final contexts
(114–220k, half tool results) compact, cutting per-call input tokens on sessions that
read big captures — this session's gate log alone is ~15k chars re-sent on every later
call. Measure: the next sessions' `input_tokens`/final-ctx rows in COST_LEDGER.md versus
the phase-2 rows (98–99% cache reads ⇒ cost tracks final ctx × calls).

## D61 (recorded, not built): the console-side capture shape

If verdict case 4 lands, the next diagnostic pass adds `/dev/console` behind
`console-dev` (off by default, R6): open arm returns a fd whose `write` prints the user
bytes to the kernel console (the `/dev/tty` fd shape; glue prints already reach it), so
`java -version > /dev/console 2>&1; echo JAVARC=$? >> /dev/console` reads the datum out
of `$W/vmrun.log` with sshd fully out of the loop. Estimated ≈30 LOC by the /dev/tty
precedent. NOT built in this gate: H1 may make it unnecessary, and no java-side or
kernel-side fix guess precedes the datum (the diagnostics rule, fourth application).
