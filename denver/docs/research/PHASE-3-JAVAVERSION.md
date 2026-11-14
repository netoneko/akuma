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

## D62 (attempt 1, 2026-11-14 ~14:55 UTC): the poller-less arm ends the 300 s hang story in 45 s — and two rig facts fell out

`sh denver/probe-javaversion.sh` attempt 1 (`denver/gates/PHASE-3-JAVAVERSION.attempt1-misattributed-hang.log`):

- **The foreground file arm RETURNED within 45 s** (the poke loop's `kill -0` failed after
  poke 1 — the ssh session was already over). Three gates of "no fault, no exit in ≥90 s"
  and a poller-less dispatch ends in under a minute: H1 confirmed in the strong direction,
  H4 confirmed too — **java exits, and quickly**.
- **The guest command surface is near-empty**: `cat /tmp/java-out.log` answers `127`
  (command not found), `command -v sleep` prints nothing — there is no `cat`, no `sleep`,
  no `command` on the ssh PATH (`/bin/busybox ls` was already known, L2). Every
  /tmp-file-based recovery ever attempted was doomed twice: once by the starvation
  (H1's poller), and again because **no guest-side reader exists to send the file back**.
- Two probe bugs found by their own output (the parse-time verdicts-lie rule, again):
  the B1 parse took `head -1` of a capture whose first line is the ssh client's own
  `[ssh] connecting to …` banner (B1 never actually ran), and the verdict chain printed
  `READABLE-HANG` while the session-return datum on the same page said the arm had
  RETURNED — attempt 1's PASS label is misattributed; the log is preserved as evidence
  and the canonical gate log is attempt 2's.

## D63 (attempt 2 = the gate capture): the reader arm was in the image all along — `jvmprobe java`

Attempt 1 eliminated the two designed paths (files unreadable guest-side; foreground
sessions can return but carry no payload). The third shape is composition, not invention:
`arm_java` (`denver/probes/jvmprobe.c`, the phase-1 workload arm) forks java, dup2's a
**pipe** over its stdout/stderr, and the parent BLOCKS on `read` — no polling, no
starvation, no guest-side file reader needed; the output comes back over ssh with an
exit report (`JVM INFO exit= signal= bytes=`) and every raw line (`JVM-INFO raw:`).

Result (`denver/gates/PHASE-3-JAVAVERSION.log`, guest d4acb339 unchanged, whole gate wall 9 s):

```
JVM INFO exit=0 signal=0 bytes=179
JVM-INFO raw: OpenJDK Runtime Environment (build 21.0.13.0.3+1-alpine-r0)
JVM-INFO raw: OpenJDK 64-Bit Server VM (build 21.0.13.0.3+1-alpine-r0, mixed mode, sharing)
JVM PASS openjdk version "21.0.13.0.3" 2026-10-24
```

**`java -version` is GREEN on the denver kernel**: exit 0, full banner, reader-arm wall
<1 s. The phase-3 capture problem is closed by instrument choice, not kernel code —
D59's H1 held: the `name` arm's 2 ms maps-poller was the entire "no fault, no exit,
output unrecoverable" standing record of HEAPRO→MEMINFO→GETCPU. The correction is
recorded in REQUIREMENTS (re-rank) and L2.

The window's census, read with x86_64 names (the D58 discipline; phase-1's census names
already agree): setitimer(38) ENOSYS ×3, membarrier(324) ENOSYS ×5, getrlimit(97) ×5,
prlimit64(302) ×5 — **all passed by a JVM that then printed its version and exited 0**;
`readlink`(89) EINVAL ×12 (buffer growing 0xfe7→0x1000 — `/proc/self/exe`-shaped, served
as a non-symlink or missing: **recorded suspect, non-fatal for `-version`**); `tkill`
(200, sig 34) with the dying-slot syscall ring = HotSpot's normal shutdown posts.
`[nosys] nr=324` also fires from the boot's own slots (s3/s4) — the akuma-private
`SET_BOX_STACK`=324 collision with x86_64 membarrier is the D58 two-table lesson made
concrete, recorded not judged. `/dev/console` write answers rc=1 (D61 stays unbuilt —
case 4 never fired). B1 was skipped (no sleep in the image — datum recorded); H2/H3 die
with H1 anyway: the channel answered pokes and the full session in the same minute.

**D61 disposition: retired as not-needed.** The console-side capture shape stays in this
artifact as the precedent map (the `/dev/tty` fd shape) for any future channel-death,
but no gate needs it today.

## D61 (recorded, not built — now retired as not-needed, see D63): the console-side capture shape

If verdict case 4 had landed, the next diagnostic pass would add `/dev/console` behind
`console-dev` (off by default, R6): open arm returns a fd whose `write` prints the user
bytes to the kernel console (the `/dev/tty` fd shape; glue prints already reach it), so
`java -version > /dev/console 2>&1; echo JAVARC=$? >> /dev/console` reads the datum out
of `$W/vmrun.log` with sshd fully out of the loop. Estimated ≈30 LOC by the /dev/tty
precedent. NOT built in this gate: H1 may make it unnecessary, and no java-side or
kernel-side fix guess precedes the datum (the diagnostics rule, fourth application).
