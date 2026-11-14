# Autoresearch experiment results: denver (a JVM on a platform kernel), 2026-11-13..14

What a cheap model in a gated loop produced, measured against the kernel it was built on. Companion to
`CLOJURE_AND_JAVAC_ON_DENVER.md` (what the result can do). Numbers are measured unless marked; the method and
its limits are in the last two sections. One run, one model, one box: an experiment report, not a benchmark.

## The experiment

- **Goal:** boot a JVM (JDK 21.0.13, G1, JIT on) as a guest of the denver kernel, a platform kernel built over
  `akuma-tokio-kernel`, on the ryzen box under Akuma's `/dev/kvm0`. Denver is the "Shanghai pattern's next
  student" of Taoxichuan (the kernel the minimal-self-hosting experiment produced).
- **Agent:** GLM-5.3-Flash through `goose`, on a z.ai **lite** plan (5 h window about 35-38M tokens, weekly about
  190M). Driver `denver/autoresearch.sh`: one gate per session, a probe that yields `RESULT PASS|FAIL`, bookkeeping
  (`LOOP_LOG.md` `### ` entry, `NEXT GATE:` line, gate log, `RESULT.md`), commit and push to `litter denver`.
- **Operator:** directs, relaunches, answers; does not write the gate code. Claude Code babysat the driver and made
  the operator-side changes listed under Interventions.

## Outcome

| Milestone | State |
|---|---|
| Hello World (hand-assembled class) on an unchanged kernel | green (loop gate) |
| Real-javac `Workload`/`Worker` (exceptions, file I/O, 24 x 16 MiB churn, thread) | green (loop gate JAVACLASSLIB, 17 s) |
| `javac` compiling and running programs **inside the guest** | works (operator-run; the loop was cut off before its own gate) |
| Clojure 1.12.0 (runtime bytecode generation, `pmap`) | works (operator-run, on the loop's kernel, no code change) |
| Scala 2.13.14: `scalac` compiles a program in the guest and the class runs | works (operator-run, one run, ~3 s compile) |
| Groovy 4.0.22 runs a script in the guest | works (operator-run, one run) |
| Standard HTTP server (JDK `com.sun.net.httpserver`, from Clojure) serving a response to a client outside the guest | works (operator-run, one run): HTTP 200 from the Mac. First blocked by `getrlimit`/`prlimit64` not served; fixed by enabling the `shell` syscall set (one-line feature relay, 88 B) |
| `sigprobe` (does `rt_sigreturn` restore an edited `rax`?) | FAIL, unexplained; javac and Clojure do not need it |

## Where it ran: Akuma all the way down

The whole loop ran **on Akuma**, on the ryzen box booted into the KVM-herd entry (`/dev/kvm0`): `goose` and the
driver, `git`, `cargo`/`rustc` (the denver kernels were built there, about 27 s per build), the gate probes, and
every guest the agent debugged (`herd vm-run`, one VM at a time on `/dev/kvm0`) were Akuma processes. The agent's
debugging loop was: build a kernel on Akuma, boot it as a guest of Akuma's own hypervisor, read the guest console, fix,
repeat. Nothing in that loop ran on Linux or macOS. What did run elsewhere: the operator's Mac (one real `javac`
for the staged classes, the `Logo`/`Workload` cross-checks, the Maven downloads) and Claude Code's ssh into the box.
So the hypervisor, the toolchain and the JVM's host were all the system under test's own family, which is also why
a host-kernel fault (box wedges, one reboot into the KVM entry, a GRUB index trap) showed up as a loop intervention.

## Size: denver against taoxichuan (linked production Rust)

Workspace crates in each binary's dependency closure (`cargo tree -e normal`, target `x86_64-unknown-none`),
non-blank non-comment lines of `src/` minus tests, `scripts/benchmarks/linked_loc.py`:

| | crates | code lines | doc-comment lines |
|---|---|---|---|
| denver (gate feature set) | 90 | 68,642 | 35,362 |
| taoxichuan (default) | 89 | 68,637 | 35,340 |
| shared by both | 88 | 68,624 | - |
| denver-only crates | 2 | **18** (`denver` 13, `akuma-jvm-kernel` 5) | |
| taoxichuan-only crate | 1 | 13 | |

Largest shared crates: `akuma-syscalls-glue` 13,272, `akuma-exec` 6,434, `akuma-tokio-kernel-amd64` 3,068 (before
the loop's `faultsig.rs`), `akuma-ext2` 2,958, `akuma-mmu` 2,703, `akuma-threading` 2,618. Denver is taoxichuan plus a
front crate and 18 own lines; **the loop did not write an operating system, it taught an existing one to run a JVM.**
Gate feature set: `wx-relaxed,vsyscall,procfs-diag,trace-err,fold-regpreserve,lazy-release-sweep,sysinfo,getcpu,fault-signal` (the operator later added `shell`).

Built kernels (box, `--release`): denver default features 2,452,232 B, taoxichuan 2,452,512 B (text 1,578,485 vs
1,578,986: denver default is 501 B **smaller**, the R6 "default build unchanged" claim holds); denver gate set
2,523,312 B (text 1,636,317, +57.8 KiB or +3.7%; bss +72 KB).

### The JDK against the kernel (disk size)

Measured in the guest (`du`, `ls -l` under `/opt/jdk`, JDK 21.0.13 Alpine) against the kernel binaries above:

| | size | vs denver gate kernel (2,523,312 B) |
|---|---|---|
| denver kernel, default features | 2,452,232 B (2.34 MiB) | 0.97x |
| denver kernel, gate feature set | 2,523,312 B (2.41 MiB) | 1x |
| `lib/server/libjvm.so` (HotSpot) | 20,219,160 B (19.3 MiB) | 8.0x |
| `lib/modules` (class library) | 142,774,572 B (136 MiB) | 56.6x |
| `classes.jsa` / `classes_nocoops.jsa` (CDS) | 14.6 MB / 15.0 MB | about 6x each |
| all of `/opt/jdk` (106 files, `bin/` 88 KiB) | 193,303 KiB by `du` (about 189 MiB) | **78x** |
| `denver-jdk.img` (whole root filesystem) | 671,088,640 B (640 MiB) | 266x |

The kernel is about 1.3% of the JDK it hosts; HotSpot alone is 8x the kernel. Caveats: `du` counts disk blocks (a
little above the sum of file sizes; the per-file rows are exact), the kernel is a stripped static binary (text
1.58 MB, bss about 2 MB) and the JDK is a runtime with its libraries, and the image holds more than the JDK.

### What the loop added

69 commits (subject `denver...`, non-merge, through `92c55737`; includes the operator's brief edits and checkpoints):

| Language | lines added |
|---|---|
| Rust (`.rs`, 32 files) | **1,823 added, 259 deleted, net 1,564** |
| C (probes) | 975 |
| shell (gate probes, driver) | 3,788 |
| Python (assemblers, verifiers) | 1,984 |
| Markdown (records, research) | 6,483 |

Rust by crate: `akuma-tokio-kernel-amd64` 998 (`faultsig.rs` 498), `akuma-exec` 263, `akuma-procfs` 95,
`akuma-syscalls-mem` 78, `akuma-tokio-kernel-core` 59, `akuma-vfs-glue` 51. Net 1,564 lines is about **2.3% of the
linked production Rust**, all behind off-by-default features. About 410 lines were fixes that unblocked java (W^X,
vDSO, null-store, heap, meminfo), about 650 were diagnostics to name the crashes (procfs, null-store tables),
`getcpu` 126, the signal port 498. Surrounding code is 3-4x the Rust: the probes and records are most of the work.

## Cost

| | |
|---|---|
| Elapsed | first session 2026-11-13 17:05 UTC to quota cutoff 2026-11-14 17:11 UTC: **24.1 h** |
| Session wall time (summed) | **822 min = 13.7 h** over 23 goose sessions (rest: waits, rate-limit windows, bookkeeping retries) |
| Input tokens | **172.2M** accumulated, 98% cache reads (169.2M): cost is context size x calls |
| Output tokens | **1.70M** |
| Tool calls | **2,125** |
| Quota | weekly window at 95% (187.1M by z.ai's count) at the cutoff; the 5 h window hit 100% at the final cutoff (earlier cutoffs were waited out by the watcher) |
| Money | not measured (flat subscription; no per-token bill) |

Per session, from `denver/docs/COST_LEDGER.md` (selection): SCAFFOLD 27 min / 4.8M, WX 28 / 4.6M, VDSO 62 / 19.3M
(context 220k), PROCFS 73 / 17.5M, NULLSTORE 76 / 17.7M, HEAPRO 65 / 16.5M, GETCPU 48 / 10.7M, JAVAVERSION 23 / 3.5M.
Three of the 23 sessions were operator-directed (`test2` 5 min, `merge-main` 35 min, `improve` 26 min). Sessions that
read whole large docs ended at 175-220k context and cost 3-5x the lean ones (`EFFICIENCY.md`, `STATE.md` came from this).

Cost per milestone is mostly the order the blockers fell: PROCFS + NULLSTORE (diagnosis of two crashes, four sessions) were about 42M
of the 172M; the final session (corrected brief) cost 14.8M and produced the JAVACLASSLIB green plus the signal port.

## Interventions

`denver/RESULT.md` carries **31** `INTERVENTION` lines; only 2 are labelled operator (after FAULTNAME, a driver
stop; the `litter/main` merge `3531cf69`). The file does not say who wrote the other 29 and I did not audit
authorship: they record probe defects, environment facts (`cargo` is at `/usr/local/rust/bin`, no `cat`/`sleep` in the
guest, gitignored `denver/gates/`), and cut-off sessions. Operator-side actions I can enumerate (not a complete count of
everything before the last day):

- relaunched the driver after every 5 h rate-limit cutoff and bookkeeping FATAL (the watcher automated this)
- merged `litter/main` into `denver`, verified three gate kernels; ran an `improve` session (`bookkeep.sh`,
  driver changes, `EFFICIENCY.md`)
- fixed the guest's interactive ssh (sshd from main, `--shell-arg +m`; the cause was `getpgid` nr 121 ENOSYS)
- **two brief corrections on 2026-11-14:** point the loop at real-javac classes; make javac its last priority
- staged real-javac `Workload.class`/`Worker.class` at `/root/denver/javac/`; benched one session at call 78
- verified javac and Clojure by hand after the quota cutoff

## What mattered most

1. **Ask for the tool instead of building it.** The loop hand-assembled class files (a 520-line assembler, a
   verifier, a disassembler: the pre-correction JAVACLASSLIB session, 38 min, 5.6M tokens, ended in a FAIL) because "no javac exists in
   the lineage", when `ESCALATE.md` existed for exactly that question. One real javac on the operator's Mac produced
   classes that ran on the unchanged kernel first try. After the correction the next session closed JAVACLASSLIB and
   committed the signal port within 38 min.
2. **A named hypothesis in the brief paid off.** The javac crash was a NULL read in JIT code; the brief said HotSpot
   relies on SIGSEGV delivery. The loop ported the metal kernel's fault-to-signal path 1:1 and javac then ran.
3. **The cheap model is context-bound, not capability-bound.** 98% cache reads: the savings came from lean reading, not
   from fewer calls.

## One-sentence summary, and how not to overstate it

"About a day of elapsed time, a handful of operator interventions and most of a weekly z.ai lite quota took an
existing kernel from 'java will not start' to javac and Clojure running in the guest." Corrections, so the sentence is
not read as more than it is:

- **24 h is elapsed time.** The agent worked about 13.7 h of it; the rest was waiting out rate-limit windows and
  relaunching the driver.
- **"A couple of interventions" undercounts.** The two operator corrections (real javac; javac first) mattered most,
  but the list above is longer (merge, `improve` session, sshd fix, relaunches, staging, benching, final checks), and
  `RESULT.md` has 31 `INTERVENTION` lines, 2 labelled operator.
- **Quota:** about 95% of the weekly window by z.ai's count (187.1M); the denver sessions are 172.2M of that, the rest
  is other goose use.
- **It is not a new kernel.** Denver is taoxichuan plus 18 own lines; the loop's net change is about 1,560 Rust lines
  (2.3% of the linked code), all behind off-by-default features. A mature stack plus a gated loop is what made this
  possible.
- **Clojure was an operator test, not a loop gate.** It ran on the loop's kernel with no code change; the loop's own
  javac gate never executed (quota cutoff).
- **The operator hint was load-bearing.** Without the real-javac correction the loop was still debugging its own
  class-file assembler.

## Caveats

- Linked-line counts are the **crate closure**, not symbols: unreachable or feature-gated code inside a linked crate
  counts, registry crates (about 18 per kernel) do not, doc comments are separate. The counter strips comments and
  `#[cfg(test)]` items heuristically.
- Token totals are sums of `accumulated_*` columns in goose's `sessions.db` for sessions named `denver-*`; the z.ai
  weekly figure (187.1M) also includes other goose use that week.
- javac and Clojure are operator runs, one each; the loop's PHASE-3-JAVAC gate and `LOOP_LOG` entry do not exist.
- `sigprobe` FAIL and the unrun `-Xint` arm are open. Interventions are an incomplete count.

## Ideas for the future

Not done, not scheduled; written down so the next person (or the loop, after the quota resets) can pick them up.

- **A symptom/feature matrix, generated from code.** Today a symptom (`Function not implemented` from
  `IOUtil.fdLimit`, an ssh shell that spins, `Too small maximum heap`) leads to a `[nosys] nr=N` line, and from there
  to a gated syscall set nobody listed in one place (lesson L5). Partial precedent: `docs/README.md`'s "I see X, what
  do I read?" matrix points at docs, not at build features; `denver/docs/REQUIREMENTS.md` has gap and evidence but no
  feature column. `akuma-runtime-tokio` has one function per feature (`shell(call)`, `net(call)`, ...), each a
  `matches!` over syscall names; a script could read those sets plus the console's `[nosys]` numbers and print
  `syscall | serving feature | enabled by denver?`, so the table cannot go stale. Not checked: how many such sets there
  are and whether each maps cleanly to a Cargo feature. Seed rows (verified once, by the operator): `shell` for
  97/302 (JDK HTTP server) and 121 (interactive ssh); `sysinfo` for 99; `getcpu` for 309; `fault-signal` for JIT
  null-check faults (partly: the `rax` edit still fails).
- **Diff sibling feature sets automatically.** A derivative kernel built with fewer defaults than its sibling
  (denver `--no-default-features` against taoxichuan's `default = ["shell"]`) can compare the two feature lists at
  build or gate time and print what the sibling serves that this one does not.
- **Make the loop ask before it builds.** The loop built a 520-line class-file assembler because "no javac exists",
  when `ESCALATE.md` was the place to ask; the brief now says so, but a gate-time check ("a missing external tool is an
  escalation, not a build") would not depend on a human noticing.
- **Compare the accumulated `[nosys]` list to the feature sets the loop declined**, once per phase. The loop was
  minimal on purpose (one gated arm per measured gap); a periodic sweep would have found that seven numbers shared one
  gate.
- **Extract signal delivery into a crate both kernels use.** The fault-to-signal path lives inside
  `akuma-kernel-amd64` (`signal.rs`); `akuma-syscalls-signal` is bookkeeping only. The loop had to port it 1:1 into the
  tokio kernel (498 lines); a shared delivery crate would remove the second copy and may fix the open `sigprobe` `rax`
  edit once.
- **Run the loop's own PHASE-3-JAVAC gate** and close the open items: `sigprobe`'s `rax` edit, the R6 default-`.text`
  check for the fault-signal and shell kernels, and a repeat run of every operator measurement.

## Reproduce

    python3 -I scripts/benchmarks/linked_loc.py denver --no-default-features \
      --features wx-relaxed,vsyscall,procfs-diag,trace-err,fold-regpreserve,lazy-release-sweep,sysinfo,getcpu,fault-signal \
      --vs taoxichuan
    sh denver/bookkeep.sh --cost          # newest session; the ledger is denver/docs/COST_LEDGER.md

## Background

- `denver/docs/archive/CLOJURE_AND_JAVAC_ON_DENVER.md`, `denver/docs/EFFICIENCY.md`, `denver/RESULT.md`,
  `denver/docs/LOOP_LOG.md`, the appendix below (the operator log)

## Appendix: operator log (2026-11-14)

The human's notes, kept apart from `LOOP_LOG.md` and `RESULT.md` while the loop ran (the driver's bookkeeping checks
those files, so free text there could trip it). Merged here from the branch `denver-operator-notes`
(`denver/docs/OPERATOR_LOG.md`, last commit `a548bc65`) when the loop stopped; the entries are as written then, plus
the night entry at the end. The loop never read or wrote them.

### 2026-11-14 — a real Java program runs on the denver kernel

The operator logged in to the denver guest over ssh from a Mac and ran Java by hand. In the operator's
words: "oh my god it does work", and they asked for it to be noted here that they are **extremely
impressed**. The run is the work of the GLM-5.3-Flash loop (about 17 gate sessions on 2026-11-13/14),
directed and not edited by the operator.

What was checked by hand, outside the loop's own probes:

- `java -cp /work Hello` and `java -version` run in the KVM guest (kernel `d4acb339`, OpenJDK
  21.0.13, mixed mode, G1, 4 CPUs, 3 GiB, JIT on: `-XX:+PrintCompilation` shows C1 compilations).
- `Logo.java` (the Akuma banner, `userspace/sshd/src/banner.txt`) compiled with a real `javac --release 21`
  on the Mac, then run on the Mac and on denver: **identical output, 801 bytes, same sha256**.
- An interactive ssh session (pty) into the guest from the Mac works, with `sshd` built from `main`
  (`9f623b22`) and the shell started with job control off.

Findings made while doing it (kernel gaps for the loop; the operator has not edited kernel code):

1. **`getpgid` (nr 121) answers ENOSYS.** (*Resolved the same evening: see the last entry below; the `shell` feature serves it.*) busybox `ash` on a tty spins on it at startup, so an interactive
   shell hangs with no prompt. Workaround: `sshd --shell-arg +m`. The sshd version was not the cause.
2. **The guest's uptime is the host's uptime.** Guest `/proc/uptime` read 49569.79 s while the box's was
   49584.46 s, for a guest booted minutes earlier. The same clock explains `sysinfo uptime=31956`
   in PHASE-2-MEMINFO (read there as "the clock hook IS registered"), and probably sshd's idle watch
   declaring a live session silent for 49278 s (not proven; workaround `--idle-timeout-secs 0`).
3. **javac crashes.** The runtime carries `jdk.compiler` (69 modules incl. jshell, javadoc); only the
   `bin/javac` launcher is missing. `java -m jdk.compiler/com.sun.tools.javac.Main -version` dies:
   `[Fault] ring3 PageFault rip=0x6000118848e3 cr2=0x8 code=0x4 -- task killed`, instruction bytes
   `44 8b 52 08` = `mov 0x8(%rdx),%r10d` with `rdx=0`, inside an `rwxs` mapping (the JIT code cache).
   Hypothesis, unverified: an implicit null check in JIT code. HotSpot expects the kernel to deliver
   SIGSEGV to its handler and turns it into a NullPointerException; the kernel killed the task instead.

Operator setup, for reproducing (no secrets here): separate image copy `/root/denver/w/denver-dev.img`
(not the loop's `denver-hello.img`), `vm-dev.spec` (port 2248, `epoch=` set to the current time, which
the guest uses as its initial date), `sshd` from `main`. One guest at a time: the loop's gate scripts
begin with `echo stop > /dev/kvm0` and will drop an operator session.

### 2026-11-14 (later) — operator correction given to the loop

While the loop was in PHASE-3-JAVACLASSLIB writing a bytecode assembler (`mkworkload.py`, 520 lines),
the operator gave it a correction through the session brief (`scripts/kvm/goose/denver-01-autoresearch.md`,
section "Operator correction (2026-11-14)"; committed by the driver with the next docs commit). This is a
deliberate operator hint to the experiment and is recorded here so the run stays honest about it:

- Content: it could have escalated for a real `javac`; the operator checked and staged real-javac sources and
  classes at `/root/denver/javac/` on the box (not in git); suggested using them instead of the assembler.
- The measurement behind it: real-javac `Workload`/`Worker` run on kernel `d4acb339` (unchanged) with
  `java -Xms64m -Xmx1536m -cp /work Workload`: exit 0, all markers incl. `WORKLOAD-ALL-PASS`. The loop's own
  first JAVACLASSLIB gate (assembler classes) failed with `exit=1`, markers missing. One run each; the
  inference "the classes, not the kernel" is not proven.
- Takes effect from the NEXT session's brief; the running session had already built its brief.

### 2026-11-14 (later still) — priority change given to the loop: javac is its last stretch

The operator decided the rest of the project will be finished with other models and that a working `javac`
inside the guest is realistically the GLM loop's last stretch, with about one session of weekly quota left.
The session brief (`scripts/kvm/goose/denver-01-autoresearch.md`, section "Operator priority change") now tells
the next session to: add the javac crash to `REQUIREMENTS.md` itself and rank it first; close JAVACLASSLIB with
the staged real-javac classes if they pass; spend its remaining quota on the javac gate; write records early.
It carries the fault record and the unverified hypothesis (implicit null checks need SIGSEGV delivery to a user
handler), and a definition of done: javac compiles a `.java` in the guest and the class it makes runs.
Like the earlier correction, this is a deliberate operator hint and is recorded here so the run stays honest;
takes effect from the next session's brief.

### 2026-11-14 (evening) — operator verification: javac works inside the guest

The GLM loop hit the z.ai quota (5h 100%, week 95%) at ~call 155 of the javac session, before running its own
javac gate. Its committed `faultsig` port (`bdf3a2b2`) plus its uncommitted edits (checkpointed by the operator as
`38f16b2d`) were built and run by the operator; this is an operator measurement, not a loop gate.

- Kernel: `cargo build -p denver --target x86_64-unknown-none --release` with the getcpu feature set plus
  `fault-signal` (box, `/usr/local/rust/bin`), 27 s; uname `0.1.0-38f16b2d-denver`. Rig: `/root/denver/w/denver-faultsig`
  + `denver-faultsig.img` (copy of the dev image + `sigprobe`, `Workload.java`, `Logo.java`).
- `java -m jdk.compiler/com.sun.tools.javac.Main -version` -> `javac 21.0.13.0.3`, rc 0, no `[Fault]` line
  (before: SIGSEGV at rip 0x6000118848e3, task killed).
- javac compiled `Logo.java` in the guest (1842-byte class, same size as the Mac's); the guest-compiled class
  prints output IDENTICAL to the Mac run. javac compiled `Workload.java`; the produced class ran with every marker
  incl. `WORKLOAD-ALL-PASS`. Console fault count for the whole session: 0.
- `sigprobe` reports FAIL: the SIGSEGV is delivered (si_signo=11, si_code=1, si_addr=0, ripdelta=3, cr2=0) and the
  saved-`rip` edit takes effect, but the saved-`rax` edit does not (`rc=0`, want 99). Unexplained; likely
  `rt_sigreturn` overwriting rax with its own return value. javac and the workloads do not depend on it, but other
  JVM paths might. The agent was editing `faultsig.rs`/`child.rs` when cut off.
- Not done: no loop gate/LOOP_LOG entry for PHASE-3-JAVAC, `-Xint` arm not run, default-build `.text` unchanged
  check (R6) not run, no repeat runs (one run each).

### 2026-11-14 (night) — the shell feature, an HTTP server, three more languages

Operator-run, one run each unless noted. Detail and numbers are in the sections above and in
`CLOJURE_AND_JAVAC_ON_DENVER.md`; this entry is the timeline.

- Clojure 1.12.0, Scala 2.13.14 (`scalac` in the guest) and Groovy 4.0.22 ran on the fault-signal kernel (zero `[Fault]`
  lines). Jars from Maven Central, SHA-1 checked; sources committed under `denver/samples/`.
- Env files and wrappers (`denver/samples/env/`: `java.env`, `clojure.env`, `scala.env`, `groovy.env`, wrappers `javac`
  `clojure` `scalac` `scala` `groovy`, `install.sh`) so the tools run by name from a bare ssh session; the JDK's launchers
  are symlinked into `/usr/local/bin`.
- The standard JDK HTTP server (from Clojure) first died with `Function not implemented` (`IOUtil.fdLimit`); the console's
  `[nosys]` numbers 97 (`getrlimit`) and 302 (`prlimit64`) pointed at `akuma_runtime_tokio::shell`, a syscall set
  taoxichuan enables by default and denver never did. Fix: `shell = ["akuma-jvm-kernel/shell"]` in `denver/Cargo.toml`
  (`9ec07523`), +88 B. The server then answered `HTTP/1.1 200 OK` to a `curl` from the Mac (VM spec `forward=8080:8080`),
  and a real pty ssh session ran **without** `--shell-arg +m` (finding 1 above is resolved). Lesson:
  `denver/docs/lessons/L5-find-the-feature-flag.md`.
- Process slip, recorded: the `9ec07523` commit touched a Cargo manifest but was made with `--no-verify`, so the
  pre-commit hook (clippy, tests, secret scan) did not run on it.
- State at the quota cutoff: the weekly z.ai window was 95% used, resetting Thursday 2026-11-19 16:36 UTC (18:36 IST). The
  operator will finish the remaining paperwork (`LOOP_LOG` entry for PHASE-3-JAVAC, lessons) with the loop after the reset.
  Still open: `sigprobe` `rax` edit (unexplained), the loop's own javac gate, the R6 default-`.text` check for the
  fault-signal and shell kernels.
