# Denver requirements — the capability gap table (lesson 1)

Built the way the plan's lesson 1 teaches: run the workload on the kernel, catalogue every
failure as a *capability gap* with evidence, keep suspects as hypotheses. The worked example is
the taoxichuan JVM attempt's blocker table (recovered from git `d7878378` — not on this branch;
the plan-cited path trap again). Evidence lives in `denver/gates/PHASE-1-METHOD.log` (2026-11-13,
session: PHASE-1-METHOD GREEN) — the workload `java -version` ran on **both** kernels this rig
can reach: the denver guest (the tokio-kernel platform stack) and the host Akuma metal (kernel
0.0.9, richest syscall surface available). Both runs used the same binary: Corretto 21.0.13
alpine (musl) x86_64 JRE at `/opt/jdk` (guest) = `/usr/bin/java` (metal).

**Rank = the JVM's own init order**, not judgment: HotSpot dies at the first unserved thing, so
the first blocker must close first (the attempt's order — W^X before getcpu — reproduced exactly
on the guest this session).

| # | gap | evidence (denver/gates/PHASE-1-METHOD.log) | closing pass (one per gap) |
|---|---|---|---|
| G1 | W^X policy refuses RWX and RW→R+X (`akuma-syscalls-mem::wx`; "no per-program exemption") — HotSpot's executable-page marking dies at VM init | guest: `RWX RED mmap-RWX-refused errno=13` + `RWX RED rw_to_rx mprotect errno=13`; `JVM RED Error occurred during initialization of VM reason="Failed to mark memory page as executable - check if grsecurity/PaX is enabled"` — the attempt's blocker 1, first-hand on x86_64. Metal contrast: `RWX PASS rw_to_rx=42` (kernel 0.0.9 allows it — the policy moved there; the guest stack still carries it) | feature-gated relaxation in `akuma-syscalls-mem::wx` (off by default, R6), mapped 1:1 through `akuma-jvm-kernel` → `denver` (the `console` propagation pattern, phase-0 D2). A policy change is the policy changed (R7): the feature flips the check, no per-program exemption layer |
| G2 | `getcpu(2)` served nowhere (x86_64 included) | `GETCPU RED ENOSYS` on **both** kernels; attempt's blocker 2 (HotSpot `os::init_2` exits the VM on ENOSYS) | thin arm in the amd64 machine half (validate, `copy_to_user`, errno — R4), decision in a pure crate (R3), behind the same default-off feature map |
| G3 | no vDSO: `AT_SYSINFO_EHDR` never in auxv (`akuma-elf-exec` fills `AT_PHDR`/`AT_BASE`/`AT_ENTRY`, nothing else) — on metal HotSpot then falls back to the legacy vsyscall page and SIGSEGVs inside it | `AUXV RED AT_SYSINFO_EHDR missing` (both kernels); metal `hs_err_pid*.log`: `SIGSEGV at pc=0xffffffffff600800`, RIP **inside** the unmapped page, 0.02 s into `JNI_CreateJavaVM` (gap the aarch64 attempt never reached — it died at W^X first) | phase-2 research decides: a minimal vDSO page + `AT_SYSINFO_EHDR` in `akuma-elf-exec` (the R2 home: the decision is a pure function there today) vs. provably serving the fallback address. The guest never got past G1, so this is a *suspect with metal evidence*, not a guest-proven blocker yet |
| G4 | `/proc/<pid>/auxv` (and `/proc/<pid>/maps`) not served — the fault can't be *named* once the JVM gets further (the attempt's blocker 3 died unnamed: "`/proc/<pid>/maps` is empty on this kernel") | `AUXV INFO /proc/self/auxv unreadable errno=2 (No such file or directory)` (both kernels) | render auxv/maps in `akuma-procfs` (the gated renderer the runbook says already exists — composition, R2). Diagnostics-first (R8): without maps, every later hotspot fault is a guess |
| G5 | time reads are full syscalls: no vDSO/vvar fast path | `CYCLES PASS clock_gettime=4189 cyc/call` (guest) vs `680` (metal) — a VM-trap tax on top of the missing fast path | defer: not an init blocker (`-version` died before timers mattered); revisit with a perf pass after G1–G3 if the ladder stalls on speed |

**Suspects kept as hypotheses, not gaps** (lesson 1's discipline): NULL-store-after-CodeCache
(the attempt's blocker 3) — unreachably far away until G1/G2 close; `AT_HWCAP` missing —
HotSpot may take a slower feature-detect path, no evidence of failure yet; guest syscall
round-trip cost — measured, deferred (G5).

**Probes that produced this table** (lesson 2's RED suite — all named lines, all clean,
`denver/gates/PHASE-1-METHOD.log`): `jvmprobe getcpu|auxv|cycles|rwx|java`
(`denver/probes/jvmprobe.c`). The phase-2 gate for each closing pass greps the same arm's
`PASS` line — the probes were written before any closing kernel code, as the lesson demands.

**Rank → phase-2 order:** G1 (PHASE-2-WX), G2 (PHASE-2-GETCPU), G4 (diagnostics before the
unknown: PHASE-2-PROCFS, re-rank openly if evidence changes it), G3 (PHASE-2-VDSO, research
first — the biggest unknown), G5 (deferred).
