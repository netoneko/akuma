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
Gate feature set: `wx-relaxed,vsyscall,procfs-diag,trace-err,fold-regpreserve,lazy-release-sweep,sysinfo,getcpu,fault-signal`.

Built kernels (box, `--release`): denver default features 2,452,232 B, taoxichuan 2,452,512 B (text 1,578,485 vs
1,578,986: denver default is 501 B **smaller**, the R6 "default build unchanged" claim holds); denver gate set
2,523,312 B (text 1,636,317, +57.8 KiB or +3.7%; bss +72 KB).

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

## Reproduce

    python3 -I scripts/benchmarks/linked_loc.py denver --no-default-features \
      --features wx-relaxed,vsyscall,procfs-diag,trace-err,fold-regpreserve,lazy-release-sweep,sysinfo,getcpu,fault-signal \
      --vs taoxichuan
    sh denver/bookkeep.sh --cost          # newest session; the ledger is denver/docs/COST_LEDGER.md

## Background

- `denver/docs/archive/CLOJURE_AND_JAVAC_ON_DENVER.md`, `denver/docs/EFFICIENCY.md`, `denver/RESULT.md`,
  `denver/docs/LOOP_LOG.md`, `denver/docs/OPERATOR_LOG.md` (branch `denver-operator-notes`)
