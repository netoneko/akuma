# javac and Clojure running inside a denver guest (2026-11-14)

Operator-run verification, not a loop gate. The GLM autoresearch loop was cut off by the z.ai quota before it
could run its own PHASE-3-JAVAC gate; the operator built the kernel it had committed and measured the result by
hand. Every number below is from **one run** unless stated.

## Symptom

On kernel `d4acb339` (denver-getcpu) a JDK 21.0.13 (Alpine, G1, C1/C2 JIT) ran Hello World and a real-javac
workload, but `java -m jdk.compiler/com.sun.tools.javac.Main -version` died on its first JIT-compiled null check:

    [Fault] ring3 PageFault rip=0x00006000118848e3 cr2=0x8 code=0x4 -- task killed

The faulting bytes `44 8b 52 08` are `mov 0x8(%rdx),%r10d` with `rdx=0`, inside the JIT code cache
(an `rwxs` mapping). A Hello World that never takes a null check in compiled code did not notice.

## Cause

HotSpot compiles **implicit null checks**: a read through a NULL object is allowed to fault, and the JVM's own
SIGSEGV handler turns that fault into a `NullPointerException`. Safepoint polls use the same trap. The denver kernel
killed the task on any ring-3 page fault it could not resolve and never delivered it to a user handler.

## Fix (the loop's, `bdf3a2b2` + `38f16b2d`)

`crates/akuma-tokio-kernel-amd64/src/faultsig.rs` (498 lines, plus `child.rs` edits): a port of the full amd64
kernel's fault-to-signal path (`crates/akuma-kernel-amd64/src/signal.rs`) with a Linux-ABI `SigContext`, relayed
1:1 through the front crates as the off-by-default feature **`fault-signal`** (R6: the default build is unchanged).
No crate could be reused: `akuma-syscalls-signal` is the `rt_sigaction`/`sigprocmask` bookkeeping only ("delivery
stays in the kernels"), and the tokio kernel depends on neither it nor `akuma-kernel-amd64`. A shared delivery crate
is the cleaner long-term shape.

Gate kernel feature set: `wx-relaxed,vsyscall,procfs-diag,trace-err,fold-regpreserve,lazy-release-sweep,sysinfo,getcpu,fault-signal`.

## Verification (operator, kernel `0.1.0-38f16b2d-denver`)

Build on the ryzen Akuma box (cargo is at `/usr/local/rust/bin`, not on the default `PATH`):
`cargo build -p denver --target x86_64-unknown-none --release --features <set above>`, 27 s. Guest: 4 vCPUs,
3072 MiB, JDK 21.0.13 at `/opt/jdk/bin`.

| Check | Result |
|---|---|
| `java -m jdk.compiler/com.sun.tools.javac.Main -version` | `javac 21.0.13.0.3`, rc 0 |
| javac compiles `Logo.java` in the guest | 1842-byte class (same size as the Mac's); its output is IDENTICAL to the Mac run |
| javac compiles `Workload.java`; the class runs | exceptions, file I/O, 24 x 16 MiB humongous churn, thread returning 42; `WORKLOAD-ALL-PASS` |
| Clojure 1.12.0 `clojure.main -e "(println :hello)"` | prints `:hello 1.12.0`, ~2 s wall (Mac clock) |
| Clojure script: `reduce`, recursive `fib 20`, `pmap` | `5050`, `6765`, `[0 1 4 9 16 25 36 49]` |
| Scala 2.13.14: `scalac` (scala-compiler/-library/-reflect from Maven, SHA-1 checked) compiles `HelloScala.scala` inside the guest, then the class runs | scalac rc 0 in ~3 s (Mac clock), 4 classes (object, case class, companion); output `Hello from Scala 2.13.14 on Linux amd64`, `squares: List(1, 4, 9, ...)`, `sum of evens: 30`, sorted case classes; run rc 0 |
| Groovy 4.0.22 (`groovy-4.0.22.jar`, SHA-1 checked; dynamic dispatch via `invokedynamic`, runtime class generation) runs `hello.groovy` with `groovy.ui.GroovyMain` | prints `Hello from Groovy 4.0.22 on Linux amd64`, squares, `sum of evens: 30`, sorted `[alan(41), ada(36)]`, `dynamic: CBA`; rc 0, ~1 s (Mac clock) |
| Console `[Fault]` lines across all of it | 0 |

## Clojure details

Jars from Maven Central, SHA-1 checked against Maven's own `.sha1` files, copied to the guest and md5-checked:
`clojure-1.12.0.jar` (4,227,052 B), `spec.alpha-0.5.238.jar`, `core.specs.alpha-0.4.74.jar`. Command:

    java -cp clojure-1.12.0.jar:spec.alpha-0.5.238.jar:core.specs.alpha-0.4.74.jar clojure.main hello.clj

Why it is a hard test and not a toy: Clojure compiles every form to JVM bytecode at runtime and defines the classes
through its own class loader, so it exercises runtime class definition, the JIT and thread creation far more than a
precompiled Hello World. The script's ~62 s wall time is **not** a kernel finding: `pmap` starts Clojure's agent
thread pool, which keeps the JVM alive for 60 s after the script ends unless the script calls `(shutdown-agents)`
(the same on any host).

## What is still open

- **(Resolved 2026-11-14, evening)** The standard JDK HTTP server did not start: a Clojure server on
  `com.sun.net.httpserver` (`denver/samples/clojure/server.clj`) died with `java.io.IOException: Function not
  implemented` from `sun.nio.ch.IOUtil.fdLimit` (via `EPollSelectorProvider.openSelector`). Cause: `getrlimit` (97) and
  `prlimit64` (302) are in `akuma_runtime_tokio::shell`'s syscall set, which taoxichuan enables by default and denver
  (`--no-default-features`) never did; glue already implemented both (`sys_prlimit64`). The same set holds `getpgid`,
  `getppid`, `getrusage`, `clock_getres` and `membarrier`, every `[nosys]` number seen on the guest. Fix: a one-line
  relay `shell = ["akuma-jvm-kernel/shell"]` in `denver/Cargo.toml` (`9ec07523`), off by default (R6), kernel built with
  the gate set plus `shell` (2,523,400 B, +88 B). Verified (one run): the server prints `listening on :8080` and answers
  `HTTP/1.1 200 OK` to `curl http://<box>:8080/` from the operator's Mac through the VM spec's `forward=8080:8080`
  (hit counter 1, then 2; `/json` returns `{"server":"clojure 1.12.0","hits":2,"java":"21.0.13.0.3","cpus":4}`), and a
  real pty ssh session works **without** `--shell-arg +m` (prompt, `ls`, `javac -version`, `java ...` all run), because
  `getpgid` is served now.
- `sigprobe` (`denver/probes/sigprobe.c`) reports **FAIL**: the SIGSEGV is delivered correctly (si_signo 11,
  si_code 1, si_addr 0, rip delta 3, cr2 0) and the saved-`rip` edit takes effect, but the saved-`rax` edit does
  not (returns 0, want 99). Unexplained; a likely cause, **not confirmed**, is `rt_sigreturn` overwriting `rax` with
  its own return value. javac and Clojure do not need it; other JVM trap paths might.
- Not run: the `-Xint` arm, the R6 check that the default build's `.text` is unchanged, repeat runs.
- No LOOP_LOG entry or gate log for PHASE-3-JAVAC; the loop never reached them.
- The guest clock is host uptime-based, so the guest's own `time` output is wrong (it printed 0.73 s for a 2 s run).
  Time from outside.
- Interactive ssh needed `--shell-arg +m` until the `shell` feature (above) served `getpgid`; it no longer does.

## Cost

About 1,250 added lines of Rust for "java prints Hello World" (loop gates SCAFFOLD..GETCPU, 66 loop commits), plus
498 for `faultsig.rs`: roughly 1,750 lines of kernel Rust in total, all behind off-by-default features, built on the
project's existing amd64 crates. The javac session ran on the z.ai lite plan (GLM-5.3-Flash) and ended at the weekly
quota (95%).

## Reproduce

Guest image: a copy of `denver-jdk.img` with the Akuma sshd, then stage sources over ssh with
`/bin/busybox cat > /work/X`. Applet links: `/bin/busybox --install -s /bin` (406 entries; the guest has no
`ls`/`mkdir`/`cat` links otherwise). Compile: `java -m jdk.compiler/com.sun.tools.javac.Main -d out X.java`;
there is no `javac` launcher in the runtime.

## Background

- `denver/docs/research/PHASE-3-JAVAC.md` (the loop's design: decisions D73-D76, the four-arm probe plan)
- `denver/docs/OPERATOR_LOG.md` on branch `denver-operator-notes` (operator corrections and this verification)
- `denver/docs/research/PHASE-3-JAVACLASSLIB.md` (why real-javac classes passed where the hand-assembled ones failed)
