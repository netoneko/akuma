# PHASE-3-JAVACLASSLIB — the four-surface compiled workload (G12)

Session 18, 2026-11-14. Ordered by PHASE-3-JAVAWORKLOAD's re-rank: the third rung is a
compiled workload reaching **exceptions (throw/catch/unwind), user threads (clone/join),
file I/O through the java.io path, allocation pressure through G1** — staged through the
same debugfs shape, read through the same pipe arm, kernel unchanged unless a capture
names a death. This session also carries the **operator priority change (2026-11-14)**:
the `javac` SIGSEGV inside the guest becomes the ladder's top requirement (see
`denver/docs/research/PHASE-3-JAVAC.md` and REQUIREMENTS.md G13); this gate closes first
so the ladder re-points, because its inputs are already in hand.

## Correction of D64's premise — "no javac anywhere in the lineage" was a question, not a fact

D64 (last gate) decided "there is no javac anywhere in the lineage" from an **R9 repo
search** (`docs/archive/`, `docs/reference/`, `shanghai/`, `taoxichuan/`) — correct as a
repo search, wrong as an existence claim: the lineage also includes **the operator**. The
first JAVACLASSLIB attempt built `denver/probes/mkworkload.py` (a bytecode assembler)
instead of asking. The operator correction (2026-11-14) names the lesson: *that was a
question to ask, not a reason to build* — `denver/ESCALATE.md` (`QUESTION:`) is the
channel. Recorded, not judged; the lesson's Do/Record sections carry it.

What actually exists (operator-measured):
- A real `javac` — Homebrew OpenJDK **26.0.2 on the operator's Mac only** (not on this
  box), used as `javac --release 21` → class major version 65, compiler-generated
  StackMapTable frames.
- Staged for this session in `/root/denver/javac/`: `Workload.java` / `Worker.java`
  (same four surfaces and markers as `mkworkload.py`) plus the compiled classes.
- **The operator ran the compiled classes on the denver kernel d4acb339 unchanged** with
  this gate's own command line (`/opt/jdk/bin/java -Xms64m -Xmx1536m -cp /work Workload`):
  **exit 0, every marker including `WORKLOAD-ALL-PASS`**. The kernel is not what failed
  the first JAVACLASSLIB attempt (`exit=1`, markers missing); the difference is the class
  files. That is the operator's inference from one run — this gate's log makes it
  first-hand.

**Decision D69: this gate's class files are the operator-staged real-javac classes.**
Not the assembler: the operator supplies compiled classes on request (escalation), the
premise of debugging `mkworkload.py` (no other source of class files) is gone, and the
quota for this model is one session — spend it on the javac crash. The assembler stays in
`denver/probes/` as a recorded instrument with its known-good `Hello.class` output; its
correctness gap vs a real compiler is exactly what the first JAVACLASSLIB attempt hit.

## Delivery (D70) — fresh debugfs staging, stale attempt residue not carried

`/root/denver/w/denver-classlib.img` (from the failed attempt) carries the old classes
plus stray `IoV1..IoV7` staged minutes before this session. A gate log must show exactly
what ran, so this gate re-stages **fresh**: `cp denver-jdk.img denver-classlib.img`
(the proven image is never modified in place — D65), `debugfs mkdir /work`, debugfs-write
`Workload.class` + `Worker.class` from `/root/denver/javac/`, and verify by dumping the
files **back out of the image** and md5-summing against the README-recorded values
(`Workload 477ed2ef861fb248cea250b2e118fb79`, `Worker 791d69b2a6653d9c8ecc2174e55a288e`).
Spec `vm-classlib.spec` already points at this image (same proven kernel
`denver-getcpu` = d4acb339, 4 vcpus, 3072 MiB — the -Xmx1536m heap fits).

## Reader (D71) — the pipe arm only, markers parsed from the captured run

`denver/probe-javaclasslib.sh` is `probe-javaworkload.sh` with the workload swapped
(D66's reader unchanged — L2 rule, no maps poller anywhere): the arm is
`/bin/jvmprobe java /opt/jdk/bin/java -Xms64m -Xmx1536m -cp /work Workload`, alarm-bounded
so the exit report always returns. PASS = **READABLE-RUN**: `JVM INFO exit=0` AND
`WORKLOAD-ALL-PASS` captured through the pipe, AND all four surface markers
(`WORKLOAD-EXC-PASS`, `WORKLOAD-IO-PASS`, `WORKLOAD-GC-PASS`, `WORKLOAD-THREAD-PASS`)
present — a marker-only pass would hide a surface that silently failed. Platform sanity
arms kept from last gate: tokioprobe RESULT PASS, the getcpu arm (proves the whole
riding feature set), rig identity (uname carries d4acb339, no older rig hash), guest-side
`ls /work`, console window scanned for the old death rip / vsyscall death / any fault.

## What this gate forces

If PASS: G12 closes — the four surfaces are served by d4acb339 **unchanged** (zero new
kernel code for the third rung), and per the operator priority change the ladder
re-points at the javac crash: **NEXT GATE: PHASE-3-JAVAC**. The unranked suspects
(readlink 89, setitimer 38, membarrier 324, getrlimit 97, prlimit64 302) become rows
only if this bigger workload's console window names a death naming one.

If a surface fails: the console/pipe capture is the datum; the gate REDs with the named
death and the javac gate waits (records still land — records before polish).

## JAVAC gate sketch (the real spend of this session; full artifact: PHASE-3-JAVAC.md)

Datum (operator-measured, kernel d4acb339): `java -m
jdk.compiler/com.sun.tools.javac.Main -version` dies
`[Fault] ring3 PageFault rip=0x6000118848e3 cr2=0x8 code=0x4` — `mov 0x8(%rdx),%r10d`
with rdx=0, inside the **JIT code cache** (`rwxs` mapping). Hypothesis (the operator's,
not yet a finding): HotSpot compiles implicit null checks (a read through a NULL
object) and relies on the kernel delivering SIGSEGV to the JVM's own handler, which
raises NullPointerException; here the kernel kills the task instead. Diagnostics before
fixes (R8): (1) does the JVM install a SIGSEGV handler (`rt_sigaction` served? actually
registered?), (2) does this kernel deliver a **synchronous** fault to a user handler at
all (siginfo + ucontext + `rt_sigreturn`) — a static C probe can answer this without the
JVM, (3) is the crash JIT-specific (same command `-Xint`). Name the cause from the
console before changing code; kernel changes stay behind off-by-default features, default
`.text` unchanged (R6). Done means: inside the guest javac compiles a real `.java` (the
staged `Workload.java`) and the produced class runs.
