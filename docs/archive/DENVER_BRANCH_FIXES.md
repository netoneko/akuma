# Denver branch: the defects behind the JVM blockers (2026-11-13 .. 2026-11-14)

The `denver` branch builds the platform kernel (`denver/`, over `akuma-tokio-kernel-amd64`) that runs a JDK 21 guest
(HotSpot, G1, C1/C2) in KVM; a GLM-5.3-Flash loop worked it gate by gate. The whole run is in
`denver/docs/archive/AUTORESEARCH_EXPERIMENT_RESULTS.md`; this file is the **incident record** the bug-fix lists index.
Every fix below sits behind an off-by-default feature relayed `denver` -> `akuma-jvm-kernel` -> `akuma-tokio-kernel` ->
`akuma-tokio-kernel-amd64`, so the default build is unchanged (rule R6). Counting follows
`TAOXICHUAN_BUG_FIX_LIST.md`: a syscall merely missing is counted only where a named failure records it as the defect.

## Fixed, counted

**1. The legacy-syscall fold clobbered the caller's six argument registers (G9, `fold-regpreserve`).**
Symptom: `java -version` died mid-init on `[Fault] ring3 PageFault ... cr2=0x0 code=0x6`, a user write through NULL.
Cause: the dispatcher rewrote nr 4 (`stat`) into `fstatat` *in the caller's saved frame* (`[AT_FDCWD, path, buf, 0, 0, 0]`)
and the stub then popped that frame back into ring 3, so musl's saved `buf` in `r8` came back zeroed and its copy-out
stored to NULL. Linux preserves everything but `rax`/`rcx`/`r11`; the metal kernel takes arguments by value and never had
the bug. Fix: the six registers as they arrived are put back after the call (`dispatch_inner`). Verified: 20/20 register
checks, and the java arm moved past `fstatat` (`denver/docs/research/PHASE-2-NULLSTORE.md`, `PHASE-2-NULLFIX.md`; `72be5c55`,
`c588fe48`).

**2. `sysinfo(2)` was served nowhere, so HotSpot sized its heap to nothing (`sysinfo`).**
Symptom: the VM exited cleanly (`exit=1`, no signal) with `Too small maximum heap`. Cause: musl `sysconf(_SC_PHYS_PAGES)`
calls `sysinfo` (nr 99), which answered `ENOSYS`. Fix: one arm translating glue's answer, as the metal does. Verified:
`errno=38` became the guest's real 3 GiB (`PHASE-2-MEMINFO`; `0c559881`).

**3. The `shell` syscall set was never on the denver build (`shell`).**
Symptom: the JDK HTTP server died with `IOException: Function not implemented` from `sun.nio.ch.IOUtil.fdLimit`, and an
interactive ssh shell spun until started with `--shell-arg +m`. Cause: `getrlimit`/`prlimit64`, `getpgid`, `getppid`,
`getrusage`, `clock_getres`, `membarrier` are in `akuma_runtime_tokio::shell`, which taoxichuan enables and a
`--no-default-features` denver build did not; glue already implemented them. Fix: one relay line in `denver/Cargo.toml`
(`9ec07523`, +88 B). Verified (one run): `HTTP/1.1 200 OK` from outside; a pty ssh session with no `+m`. Only the two
calls above were exercised; `setsid`, `times`, `membarrier` and the id getters are enabled and untested
(`denver/docs/lessons/L6-find-the-feature-flag.md`).

**4. W^X policy refused what the JIT needs (`wx-relaxed`).**
Symptom: HotSpot's code cache (RWX, and RW -> R+X flips) was refused and the JVM died at init. Fix: pure checks answer
Allow and `WX_ENFORCE` boots off, compile-time (`8a658723`). Verified: `RWX PASS rw_to_rx=42` (`PHASE-2-WX`).

**5. No legacy vsyscall page, no vDSO: HotSpot's hardcoded `0xffffffffff600800` killed it (`vsyscall`).**
Symptom: `[Fault] ring3 PageFault rip=0xffffffffff600800 ... code=0x14 -- task killed`. Cause: `libjvm.so` embeds the
vsyscall `gettimeofday` address as an immediate, and the kernel had neither that page nor `AT_SYSINFO_EHDR`. Fix: the page
mapped user-RX from the kernel root at boot, plus an exports-free vDSO ELF disclosed in auxv (`0xffffffffff5ff000`).
Verified: `AUXV PASS`, tokioprobe `RESULT PASS` (`PHASE-2-FAULTNAME`, `PHASE-2-VDSO`; `55034310`, `4a0bf4a4`).

**6. A synchronous ring-3 fault killed the task instead of reaching the program's SIGSEGV handler (`fault-signal`).**
Symptom: `javac` died on its first JIT-compiled null check: `rip=0x00006000118848e3 cr2=0x8 code=0x4 -- task killed`.
Cause: HotSpot compiles implicit null checks and safepoint polls as faulting loads and turns the SIGSEGV into an
exception in its own handler; the kernel had no fault-to-signal delivery (`akuma-syscalls-signal` is bookkeeping only).
Fix: `faultsig.rs`, a 1:1 port of the metal's `signal.rs` with a Linux-ABI `SigContext`, restoring from the ucontext at
`rt_sigreturn` so an edited `rip` takes effect (`bdf3a2b2`, `38f16b2d`). Verified: `javac 21.0.13` runs in the guest, its
output is identical to the Mac's, Clojure, Scala and Groovy compile and run, zero `[Fault]` lines; the java-free arm of
the gate (`sigprobe`) shows `si_signo=11 si_code=1 si_addr=0` with real siginfo (`PHASE-3-JAVAC` gate PASS).

**7. The `sigprobe` binary did not match its source, and reported a kernel bug that was not there (G14, 2026-11-14).**
Symptom: `SIGPROBE FAIL rc=0 (want 99)`: the handler's `rax` edit did not survive `rt_sigreturn`, although the `rip`
edit did. It was recorded as an open kernel item (G14, `PHASE-4-SIGRAX`) and blamed on `rt_sigreturn` overwriting `rax`.
Cause: the committed binary (md5 `aa74a730...`) stores the "rax = 99" edit at ucontext byte `40 + 112`, which is the
**`rcx`** slot; its read of `rax` is at `+104`, as the source says. The source writes `+104`. Instrumenting the kernel
(register dump of all 17 saved `gregs` at delivery and at restore) showed the kernel reading `g13 (rax) = 0` and
`g14 (rcx) = 0x63`, i.e. it restored exactly what the program wrote. Fix: no kernel change; `sigprobe` rebuilt from its
source on the box (`gcc -static -O2`, md5 `7759d02e...`) and committed. Verified: on the **same kernel build** the old
binary FAILs and the rebuilt one prints `SIGPROVE rax-after=99` / `SIGPROBE PASS`. How the stale binary came to differ
from the source is not known; the gate compared the staged binary against the committed one, never against a fresh
build, so it could not notice. Lesson: a probe's verdict is a claim about the kernel only if the probe is built from the
source you are reading; disassemble a surprising probe before blaming the kernel.

**8. The AArch64 build broke on a call that exists only on the amd64 `UserAddressSpace` (shared crate `akuma-exec`).**
Symptom: `error[E0599]: no method named for_each_user_leaf found for reference &UserAddressSpace` at
`crates/akuma-exec/src/process/maps_rows.rs:149`, reported by the user building the default AArch64 kernel. Cause: the
PHASE-2-PROCFS gate moved the `/proc/<pid>/maps` hardware-leaf walk into `akuma-exec` (D27) and called
`for_each_user_leaf` unconditionally; `akuma-mmu` defines it only on the x86_64 `UserAddressSpace`, and the loop's gates
only ever built amd64. Fix: `collect_leaf_runs` gated to `x86_64`, with an AArch64 stub returning no leaf rows (region
rows only, as before the move) (`8b9354f9`). Verified: `cargo check` on the aarch64 host, on `x86_64-unknown-none` and over
the default workspace; the pre-commit hook (clippy over 143 crates, tests) passed. Not boot-tested on AArch64. The same
commit split two over-long first doc paragraphs (`akuma-syscalls-linux::sched::cpu_node`,
`akuma-syscalls-glue::sched`) that clippy's `too_long_first_doc_paragraph` rejected; the loop's commits on this branch had
never gone through the hook. A default-workspace `cargo check` in every gate would have caught both.

## Not counted

- `getcpu(2)` (`getcpu`, PHASE-2-GETCPU): served nowhere, but the gate's own diagnostics record that `getcpu` was never
  called before the blocker it was ranked against, so no named failure is attributed to it.
- `/proc/<pid>/maps` and `/proc/<pid>/auxv` (PHASE-2-PROCFS): an absent feature, needed to *name* later faults.
- `render_auxv` pattern fix (`4c40d687`): a compile error in code written the same session.
- Probe and driver defects found by the gates (verdict-parser case, a 2 ms poller that looked like a hang, a hand-assembler
  and its checker sharing a missing `interfaces_count`, a staging gap that left `Worker.java` out of the image): recorded
  in `denver/docs/lessons/L2-writing-probes.md`; none is a defect in a kernel path.

## Open

- The `-Xint` arm, repeat runs, and the R6 check that the default build's `.text` is unchanged for the `fault-signal` and
  `shell` gate kernels were not run.
- `shell` enables `setsid`, `times`, `membarrier` and the id getters untested.
- The `9ec07523` manifest commit skipped the pre-commit hook (`--no-verify`): clippy, tests and the secret scan did not
  run on it, and nothing has re-checked it since.

- A single kernel boot argument of about 110 characters (a three-jar `-cp` in `initargs=`) lost its last entry, so the
  third jar was never found (QEMU run, `overlays/denver`); a wildcard classpath avoids it. Cause not investigated.

## Background

- `denver/docs/archive/AUTORESEARCH_EXPERIMENT_RESULTS.md`, `denver/docs/archive/CLOJURE_AND_JAVAC_ON_DENVER.md`
- `denver/docs/research/PHASE-*.md` (one per gate), `denver/docs/REQUIREMENTS.md` (G1..G15)
- `TAOXICHUAN_SIGNAL_DELIVERY.md`, `TAOXICHUAN_BUG_FIX_LIST.md`
