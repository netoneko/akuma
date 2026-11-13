# Denver: how to build a custom platform kernel, from tokio to a Clojure web server (plan, 2026-11-13)

**Status: plan, not run.** Nothing in the tree yet. The worked example this plan is built on is
[`TAOXICHUAN_JVM_ATTEMPT_2026_11_09.md`](TAOXICHUAN_JVM_ATTEMPT_2026_11_09.md) — three blockers, ranked
suspects, nothing landed. Denver turns that attempt into a curriculum and the curriculum into an
autoresearch experiment: an open-weight model (first: GLM-5.3-Flash) does the development, on Akuma,
booting every guest through Akuma KVM, mostly by itself.

**Procedure lives elsewhere:** the agent iterates per
[`denver/docs/runbooks/AUTORESEARCH.md`](../../denver/docs/runbooks/AUTORESEARCH.md) (session loop,
phases, code rules R1–R8, stall protocol), started by
[`scripts/kvm/goose/denver-01-autoresearch.md`](../../../scripts/kvm/goose/denver-01-autoresearch.md).
This doc is the frozen design; the living lessons and the loop log are theirs to change, never
silently.

## The idea

Two artifacts, in the Shanghai pattern:

- **`akuma-jvm-kernel/`** — the new platform crate. Front crate + feature map over `akuma-tokio-kernel`;
  patches land in existing infra behind features, new capability code ships here. No middleware.
- **`denver/`** — the teaching kernel that consumes it (the Shanghai role), plus the gates and probes.

The finale is ambitious on purpose: **a Clojure hello world served by a web server**, on a kernel that
refused to let anything JIT at the start. The ladder gates it: `java -version` → Java hello world →
evaluate → Clojure.

## Lesson style (runbook-adjacent)

Each lesson is one page, and reads like a runbook with a verdict:

1. **Goal** — one sentence, the capability the kernel gains or the skill the lesson teaches.
2. **You have** — the state at the start: crates, docs, the probe that currently fails.
3. **Do** — numbered imperative steps, each ending in a checkable outcome; stop at the first that fails
   and write down why.
4. **Gate** — a table row: *passes when* a named probe prints a named line. One probe per lesson,
   written before the kernel code (lesson 2 makes this a rule).
5. **Record** — what was done (brief), what decision it forced, link to the full doc.

Rules that hold across every lesson: throwaway patches in QEMU until gates pass, commit only what is
proven; fix diagnostics before bugs; a better failure is progress (`hs_err` is a milestone); cite the
archive doc that motivated the lesson.

One standing design rule, enforced by the runbook (R1–R8): **reduce new code**. Every capability pass
begins with research into existing crates — the extraction history (pure `akuma-syscalls-*` crates,
thin machine-half arms, features off by default) exists so a new capability is composition before it
is code. `akuma-syscalls-signal` already remembers `rt_sigaction`; `akuma-syscalls-mem::wx` already
owns the W^X checks; `akuma-exceptions` already walks the fault path; `akuma-procfs` already gates
maps rendering; `akuma-elf-exec` already fills the auxv. Denver finds, connects, and fills only the
holes.

## Part I — the lessons (the method)

| # | lesson | teaches | gate passes when |
|---|---|---|---|
| 1 | **Collecting requirements** | run the workload on the best kernel, catalog failures as *capability gaps* with evidence; suspects stay hypotheses. Worked example: the JVM attempt doc's blocker table | `denver/docs/REQUIREMENTS.md` exists: one table, gap → evidence → closing lesson |
| 2 | **Writing probes** | every gate is a probe; write it first. `jvmprobe`, `getcpu` probe, `getauxval` probe, cycles-per-call probe, in the `mwprobe` style | every probe *runs and fails cleanly* (the RED phase) |
| 3 | **Building a kernel platform crate** | `akuma-jvm-kernel`: front crate + feature map, `entry!`/`Kernel` re-export, zero capabilities; patches go into existing infra behind per-arch features, features propagate the way `console` does today | Denver boots tokio, `uname` says denver, feature parity with the pass-through |
| 4 | **Adjusting expectations, building the loop** | the loop: QEMU boots in seconds, `-Xlog:all=info`, `console.log`, `trace-err`, one probe per hypothesis; throwaway until proven; diagnostics before bugs. Case reading: [`BUILDING_KERNEL_WITH_LOCAL_AI_PAIN_AND_SUFFERING.md`](BUILDING_KERNEL_WITH_LOCAL_AI_PAIN_AND_SUFFERING.md) | a hypothesis-to-verdict cycle measured under an hour |

## Part II — applying the loop (a log, not lessons)

One pass per capability, each entry in the lesson style's *Record* voice: what we had to do (brief),
what decision that forced (how the loop adjusted), link to the full doc in `denver/docs/`. The list is
alive by design; the requirements table from lesson 1 ranks it.

- **`getcpu`** — pure decision, aarch64/amd64 arm, `copy_to_user`/`EFAULT`. Correct on its own; lands
  regardless of the JVM.
- **W^X** — the kernel simply gains a per-process RWX allowance; the mm arms consult it, the pure
  policy crate in `akuma-syscalls-mem::wx` barely changes. No switch, no hook table.
- **Signal delivery** — route EL0 faults through user `rt_sigaction` handlers: SA_SIGINFO ucontext,
  restorer, `sigaltstack`. Gate: `hs_err_pid.log` instead of a raw `[Fault] EL0 …` kill; likely
  unmasks blocker 3 for free.
- **`/proc/<pid>/maps`** — real VMA rendering instead of the procfs-registration-gated empty file.
- **mmap conventions** — `MAP_FAILED`, never `NULL` (suspect #1); `mprotect` commits sub-ranges of
  `PROT_NONE` reservations (suspect #2). A/B against real-Linux `strace`.
- **auxv** — `AT_PAGESZ` under 64K pages vs 4K-built musl, `AT_HWCAP` advertising LSE/CRC.
- **vDSO** — kernel-owned code page in every process; `clock_gettime` off the syscall path; GC
  threads stop crawling.
- **(whatever Java hello world reveals next)**

## Finale — the ladder

1. **`java -version`** — the JVM boots at all; every capability feature is on.
2. **Java hello world** — compile off-box, run on-box: HotSpot interpreting, one real class loaded
   from a jar (`java.util.zip`, classpath resolution).
3. **Evaluate** — what does Clojure actually need? Expect: heavier `/proc` (`/proc/self/*` for
   `InetAddress`, runtime probes), thread pools (futex paths — see
   [`BUN_GC_FUTEX_HANG_2026_11_10.md`](BUN_GC_FUTEX_HANG_2026_11_10.md)), DNS (there is precedent:
   the shared `akuma-dns` client), jar loading at scale (mmap + page-cache behavior).
4. **Clojure hello world in a web server** — an AOT'd uberjar, an HTTP listener on the existing net
   stack. Gate: `curl localhost:PORT` returns `Hello, World from Clojure`.

If Clojure never runs, Part II is still a complete record of applying the loop — the archive's own
format, practiced as a course.

## The experiment: an open-weight model builds it

**Question.** With only this plan, repo access, and a shell on Akuma, can GLM-5.3-Flash (served
locally; precedent: [`GOOSE_GLM_SERVE_ON_RYZEN.md`](GOOSE_GLM_SERVE_ON_RYZEN.md)) drive Denver from
lesson 1 through the Clojure gate, mostly unattended?

**Rig.** Development on Akuma bare metal; every guest — the Denver image, the JVM image — boots under
Akuma KVM (or plain QEMU/KVM on it). The amd64 Corretto build replaces the attempt's aarch64/HVF rig:
Akuma KVM gives hardware virt, so the JVM target is x86_64 Alpine musl, same as the `sshd` images.
Gates run as the shanghai-01 brief runs them (`scripts/kvm/goose/shanghai-01-new-kernel.md` is the
format precedent: gates in order, stop at the first that fails and write down why).

**Fair rules** (owed to [`INSTALL_SCRIPT_QWEN_EXPERIMENT.md`](INSTALL_SCRIPT_QWEN_EXPERIMENT.md)):

- The plan is committed before the first agent run; it is never edited to fit a transcript.
- Gates judge, not the model's self-report: a gate passes when the probe line appears in captured
  console output, saved next to `RESULT.md`.
- Every human intervention is logged in `RESULT.md` with what was stuck and what unblocked it.
  The target is *mostly* by itself: interventions are data, not failure.
- The agent commits after each passing gate and keeps `denver/docs/LOOP_LOG.md` (Part II) current.

**What a result must look like.** `denver/RESULT.md`: each gate with its command and captured output;
the loop-log passes with their decisions; the intervention count; wall clock and token spend per
lesson. The headline is not "Clojure ran" — it is *how many passes through the loop the model needed
per capability, and which lessons (probes first, diagnostics before bugs) paid off*.

## Read in this order

| doc | what it gives you |
|---|---|
| [`denver/docs/runbooks/AUTORESEARCH.md`](../../runbooks/AUTORESEARCH.md) | the procedure: session loop, phases, R1–R8, stall protocol |
| [`denver/docs/archive/AUTORESEARCH_WORK_LOG.md`](AUTORESEARCH_WORK_LOG.md) | the machinery's diary: issues in the loop and their fixes |
| [`TAOXICHUAN_JVM_ATTEMPT_2026_11_09.md`](TAOXICHUAN_JVM_ATTEMPT_2026_11_09.md) | the syllabus: blockers, suspects, suggested order of attack |
| [`crates/akuma-tokio-kernel/README.md`](../../../crates/akuma-tokio-kernel/README.md) | the brief Denver's lesson 3 follows |
| [`shanghai/README.md`](../../../shanghai/README.md) | the teaching-kernel pattern being extended |
| [`TAOXICHUAN_WX_POLICY.md`](TAOXICHUAN_WX_POLICY.md) | the policy lesson 6's allowance patches around |
| [`scripts/kvm/goose/shanghai-01-new-kernel.md`](../../../scripts/kvm/goose/shanghai-01-new-kernel.md) | the agent-brief format the experiment reuses |
