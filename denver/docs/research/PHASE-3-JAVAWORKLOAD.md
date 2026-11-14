# PHASE-3-JAVAWORKLOAD — run a compiled class (Hello.class) on the guest

Session 17, 2026-11-14. The phase-3 ladder's second rung, ordered by the JAVAVERSION
re-rank: `-version` is GREEN on kernel d4acb339 (D63, zero kernel code), and the next
rung is the first java workload that exercises **class loading, GC threads, and the
file/classpath paths `-version` never touches**. The capture problem is closed (D59/D63:
the pipe arm `jvmprobe java` is the reader, the 2 ms name-arm poller is retired — L2's
standing rule), so this gate is a workload gate, not a capture gate: **PASS requires the
class to actually run** (exit=0 + the marker line), which is deliberately stricter than
D60's "readable fate" bar — readability was last gate's deliverable; the run is this
gate's.

## The compile problem (D64) — there is no javac anywhere in the lineage

The guest image carries the alpine Corretto 21 **JRE** (probe-image.sh: `bin` = java,
jfr, jrunscript, jwebserver, keytool, rmiregistry — no javac, no jlink). The host is the
same JRE (`/usr/lib/jvm/java-21-openjdk`, readlink-verified). Measured, not assumed:
`java HS.java` source-file mode (JEP 330) dies with "A fatal error has been detected by
the Java Runtime Environment" — the JRE image has no `jdk.compiler` module, so there is
no self-hosting compile path on either side.

**R9 search (what was searched and found):** `grep -ril 'javac|.class|bytecode|hello'`
across `docs/archive/`, `docs/reference/`, `shanghai/`, `taoxichuan/` → only generic
docs (KERNEL_HOWTO, shanghai middleware, taoxichuan mwprobe); the taoxichuan JVM attempt
(probe-image.sh's cited image-build precedent) never compiled a class — it stopped at
image building. No precedent for producing bytecode ⇒ new work, but probe-side only
(R6-clean: everything lives under `denver/`).

**Decision D64: hand-assemble the class file with a checked-in generator**
(`denver/probes/mkhello.py`), emit Java 21 (major 65) bytecode for
`public class Hello { public static void main(String[] a) {
System.out.println("HELLO-DENVER-WORKLOAD-PASS"); } }`, and **verify on the HOST JRE
before it ever touches the guest** (`java -cp <dir> Hello` on the host must print the
marker — parse-time verdicts-lie discipline applied to the generator itself). The class
is deliberately minimal in bytecode but maximal in what it exercises on the guest: app
classloader resolves `/work/Hello.class` through the classpath (stat+open — the file
paths `-version` never touches), the class is defined/verified by HotSpot, main is
invoked, and `println` crosses to a write syscall. GC threads, module init, and libjli
exec resolution ride on `-version`'s proven GREEN path unchanged.

## Delivery (D65) — debugfs write into a COPY of the proven image

Precedent: jvmprobe reached `/bin/jvmprobe` the same way (probe-image.sh's staging tree
→ mke2fs -d), and that script already uses debugfs for reads; measured today: `debugfs
-w -R "write ..."` into a scratch ext2 works and round-trips byte-identical (cmp),
e2fsck -n clean. The gate stages `denver-hello.img` = **a copy** of `denver-jdk.img` +
`/work/Hello.class` (mkdir + write), fsck-checked before boot; the proven image is never
modified in place (rig-reproducibility rule, phase-0 D4). `vm-hello.spec` = vm-getcpu.spec
with only the `disk=` line changed — **the kernel binary stays `denver-getcpu` (d4acb339)
and the uname identity check REQUIRES it and rejects every older rig hash**, exactly as
the JAVAVERSION gate did. Any kernel change would be a guess before the datum reads
(diagnostics-before-bugs): the known suspects (readlink 89 EINVAL ×12 growing-buffer,
setitimer 38 / membarrier 324 / getrlimit 97 / prlimit64 302 ENOSYS) all passed by
`-version`; each becomes a row only when a capture names a death.

## Reader (D66) — the pipe arm, nothing else

`jvmprobe java /opt/jdk/bin/java -cp /work Hello` — fork, dup2 pipe, blocking read, ZERO
polling (the L2 rule that closed G10). Its alarm(90)-child / alarm(120)-parent bounds
mean **the exit report always returns**: a hang is named by signal 14 (SIGALRM) instead
of a poke census, so the JAVAVERSION fallback arm (foreground file dispatch + pokes) is
dropped — it was the H1/H4 instrument, that question is closed, and /tmp files are
write-only on this guest anyway (no cat/sleep/command, attempt-1 datum). The
`/dev/console` probe (D61) is likewise dropped: its question was answered by
JAVAVERSION's log. One conditional diagnostic arm rides: if the run does not produce the
marker or exit=0, `java -Xlog:class+load=info -cp /work Hello` runs next through the same
pipe arm — the class-load trace names exactly which resolution step died, so a RED gate
lands with an actionable row, not a rerun.

## Staging sanity, then the verdict

Guest-side image identity BEFORE the arm: `/bin/busybox ls -l /work` (the known applet,
L2) must show `Hello.class` — kills the stale-IMAGE staging hypothesis early the way the
uname check kills stale-KERNEL staging. Console window from a byte offset at the first
dispatch: old rip `60000067cd7a` must stay absent, vsyscall `ffffffffff600800` must not
recur, `[nosys]/[err]` census dumped (the next gate's data regardless of verdict).

- **RESULT PASS** ⇔ rig identity + tokioprobe PASS + getcpu PASS (cpu<4, node=0) AND
  `JVM INFO exit=0` AND the marker line captured (`HELLO-DENVER-WORKLOAD-PASS`) — the
  rung is climbed: class loading + GC threads + classpath file paths + println, served
  end-to-end by kernel d4acb339 unchanged.
- **RESULT FAIL with a named datum** (honest-loop mapping, contrast to D60): readable
  exit≠0/signal with output, or a console fault rip ⇒ the capture works but the rung
  fails; the next gate is the fix pass for whatever the datum names. `java-not-installed`
  or missing `/work/Hello.class` ⇒ staging failed, same FAIL.
- Predictions recorded BEFORE the run: (a) GREEN — `-version` already exercised libjli
  exec resolution, libjvm dlopen, full HotSpot init; Hello adds only one classpath
  stat+open and one println; (b) if RED, the class+load trace or the console census
  names the suspect, most likely readlink(89) at a path `-version` tolerated.

## Experiment proposal

None from this session — the phase's one experiment (DENVER_COMPACT=1) was proposed by
the JAVAVERSION session; the per-phase rule allows exactly one.

## Machine notes carried (L2)
ONE VM at a time (free the slot first, stop at the end, its own pid only); wait for an
ssh round trip, never a log marker; grep console-derived text -a-tolerantly; invoke as
`sh <file>` (no `#!` exec on Akuma). Capture: `sh denver/probe-javaworkload.sh 2>&1 |
tee denver/gates/PHASE-3-JAVAWORKLOAD.log`.

## Record (2026-11-14, session 17) — D67 attempt 1: the builder and the checker shared the blind spot

Attempt 1 (`denver/gates/PHASE-3-JAVAWORKLOAD.attempt1-classformat.log`, wall 11 s): `JVM RED
Error: LinkageError … reason="java.lang.ClassFormatError: Field "<init>" in class Hello has
illegal signature "()V""`. HotSpot read `fields_count` as `interfaces_count` — the u2 between
`super_class` and `fields_count` (JVMS 4.1) that BOTH `mkhello.py` and `verifyclass.py` omitted
— and parsed the methods table as fields. The structural verifier passed a structurally invalid
file: **a checker that shares the author's assumptions verifies nothing**. It still caught two
earlier bugs pre-boot (ldc operand width; Code attribute missing max_stack/max_locals/
code_length), so the read-back practice earns its keep — its limit is exactly the author's own
blind spot. Prediction (b) was wrong in an instructive way: the death was named not by the
console census or readlink(89) but by the JVM's own ClassFormatError, delivered by the pipe arm
in <1 s — **the receiver is the verifier**; a class-file defect costs one 11 s boot, not a
debugging session.

## Record — D68 attempt 2: GREEN, and the suspects re-measured non-fatal under a real workload

`denver/gates/PHASE-3-JAVAWORKLOAD.log` (wall 9 s): `JVM INFO exit=0 signal=0 bytes=27` +
`JVM PASS HELLO-DENVER-WORKLOAD-PASS` — class loading from a classpath directory, HotSpot
verification, main invocation, println; kernel d4acb339 unchanged, zero new kernel code. The
console window re-measured the suspect list under real class loading: readlink(89) EINVAL ×13
(same growing-buffer shape, still non-fatal — libjli's exec-name resolution evidently falls
back cleanly), setitimer(38), membarrier(324) double-fire, getrlimit(97), prlimit64(302) — all
present, none fatal, **still hypotheses, not gaps**. Attempt 1's diagnostic arm (class+load
trace) showed the CDS archive serving shared classes ("source: shared objects file") — the
mapped archive works on this kernel. Next rung: PHASE-3-JAVACLASSLIB — a compiled workload
exercising exceptions, user threads, file I/O, and allocation pressure; the only way the
unranked suspects become rows is a workload that reaches them.
