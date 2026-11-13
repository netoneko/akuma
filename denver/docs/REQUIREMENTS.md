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
| G1 | ~~W^X policy refuses RWX and RW→R+X~~ **CLOSED 2026-11-13, PHASE-2-WX GREEN** (`denver/gates/PHASE-2-WX.log`: `RWX PASS rw_to_rx=42` first-hand; the `wx-relaxed` feature, off by default, relayed 1:1 `denver`→`akuma-jvm-kernel`→`akuma-tokio-kernel`→`akuma-tokio-kernel-amd64`→`akuma-syscalls-mem`; tokioprobe sane under it) | guest: `RWX RED mmap-RWX-refused errno=13` + `RWX RED rw_to_rx mprotect errno=13`; `JVM RED Error occurred during initialization of VM reason="Failed to mark memory page as executable - check if grsecurity/PaX is enabled"` — the attempt's blocker 1, first-hand on x86_64. Metal contrast: `RWX PASS rw_to_rx=42` (kernel 0.0.9 allows it — the policy moved there; the guest stack still carried it) | **done**: feature-gated relaxation in `akuma-syscalls-mem::wx` (off by default, R6), mapped 1:1 through `akuma-jvm-kernel` → `denver` (the `console` propagation pattern, phase-0 D2). A policy change is the policy changed (R7): the feature flips the check, no per-program exemption layer |
| G2 | `getcpu(2)` served nowhere (x86_64 included) — **still true, but proven NOT the next init blocker**: the PHASE-2-WX capture's console census never shows `nr=168` — HotSpot dies before `os::init_2` would call it | `GETCPU RED ENOSYS` on **both** kernels; attempt's blocker 2 (HotSpot `os::init_2` exits the VM on ENOSYS) | thin arm in the amd64 machine half (validate, `copy_to_user`, errno — R4), decision in a pure crate (R3), behind the same default-off feature map |
| G6 | ~~the workload's next blocker is an unnamed SIGSEGV~~ **CLOSED 2026-11-13, PHASE-2-FAULTNAME GREEN** (`denver/gates/PHASE-2-FAULTNAME.log`: the console names the death first-hand — `[Fault] ring3 PageFault rip=0xffffffffff600800 rsp=0x00006000014fe768 cr2=0xffffffffff600800 code=0x14 -- task killed` + `[Fault] slot 5 killed (SIGSEGV)`; `FAULT PASS … module=legacy-vsyscall-page`; `RESULT PASS`, two runs byte-identical on the fault line, rig = the WX binaries unchanged). **Correction recorded (research Finding 0):** the naming line predates the gate — `git log -L` shows it in `aafbb973`, the kernel file's first commit — and the "without an address" claim below was a misquote of the WX census (`/root/denver/w/vmrun-diag.log` carries the ring3 line one line above the slot line); what was actually missing was the *capture* (probe scripts carried ssh-side output only) and the address→module step. Zero kernel/crate LOC (D17) | `JVM RED signal=11 first=""` ×3; death window: last ENOSYS calls `setitimer(38)`, `membarrier(324)`, `sysinfo(99)`, `prlimit64(302)`, `getrlimit(97)` (uid/gid family 102/104/105/106/110 earlier) — `getcpu` never called | **done**: capture + interpretation, not new kernel code — the delivery path already printed rip+cr2 (`aafbb973`); `denver/probe-faultname.sh` windows the console log per arm (byte offsets into `$W/vmrun.log`), greps it `-a`-tolerantly, and names the module at ABI level (the legacy vsyscall page = the metal `hs_err` pc); VMA-level module naming stays G4 (`/proc/<pid>/maps`, `akuma-procfs` composition) |
| G3 | no vDSO: `AT_SYSINFO_EHDR` never in auxv (`akuma-elf-exec` fills `AT_PHDR`/`AT_BASE`/`AT_ENTRY`, nothing else) — on metal HotSpot then falls back to the legacy vsyscall page and SIGSEGVs inside it | `AUXV RED AT_SYSINFO_EHDR missing` (both kernels); metal `hs_err_pid*.log`: `SIGSEGV at pc=0xffffffffff600800`, RIP **inside** the unmapped page, 0.02 s into `JNI_CreateJavaVM` (gap the aarch64 attempt never reached — it died at W^X first) | phase-2 research decides: a minimal vDSO page + `AT_SYSINFO_EHDR` in `akuma-elf-exec` (the R2 home: the decision is a pure function there today) vs. provably serving the fallback address. Written when the guest had never got past G1; **now guest-proven** (PHASE-2-FAULTNAME GREEN, 2026-11-13): the guest console names the death at rip=cr2=`0xffffffffff600800`, code=0x14 (user instruction fetch on a not-present page) — the same pc and mechanism as the metal `hs_err`, first-hand (`denver/gates/PHASE-2-FAULTNAME.log`) |
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

**Re-rank 2026-11-13, after PHASE-2-WX GREEN (recorded reason, as this file requires):** G1
closed; the workload's first blocker moved to an *unnamed* SIGSEGV (G6, new row above) that
kills the JVM before it ever calls `getcpu` — closing G2 alone would not unblock the ladder
while the death is unreadable, and HotSpot's own report path produces nothing for it (no
stderr, no `hs_err`, no core, 3 boots). The runbook's own rule applies ("fix diagnostics before
bugs: a pass that makes the failure readable outranks a pass that guesses at the fix"), and the
attempt doc's declared next step was exactly this diagnostic ("a fault report that names the
module … or the fault handler printing the VMA"). **New order: G6 (PHASE-2-FAULTNAME, kernel
names rip/cr2 on user SIGSEGV) → G2 (PHASE-2-GETCPU) → G4 (PHASE-2-PROCFS) → G3
(PHASE-2-VDSO) → G5.** G4 stays ahead of G3 (diagnostics before the unknown); FAULTNAME rides
ahead of PROCFS because the current fault dies before HotSpot's report path can read maps at
all.

**Re-rank 2026-11-13, after PHASE-2-FAULTNAME GREEN (recorded reason, as this file requires):**
G6 closed with zero kernel code — the delivery path had named the death since the kernel's
first commit; the gate captured it first-hand and turned the address into a module at ABI
level. The death is G3's mechanism with **guest evidence** (rip=cr2=0xffffffffff600800,
code=0x14), so G3 takes first place: it is the proven first blocker now, and closing `getcpu`
(G2) cannot unblock a ladder whose workload dies before `os::init_2` would ever call it
(twice-proven, phase-1 + PHASE-2-WX census). G4 (maps) moves behind G2: the next *unknown*
(the attempt's blocker-3 NULL store) only arrives once init gets further, and the vDSO gate's
own arm (`AUXV RED AT_SYSINFO_EHDR missing`) judges without maps. **New order: G3
(PHASE-2-VDSO, research first — vDSO page + `AT_SYSINFO_EHDR` in `akuma-elf-exec` vs.
serving the fallback) → G2 (PHASE-2-GETCPU) → G4 (PHASE-2-PROCFS) → G5 (deferred).**
