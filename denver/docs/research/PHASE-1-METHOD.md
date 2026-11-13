# PHASE-1-METHOD — research (R1, written before any code)

**Phase 1 = the method** (runbook): lesson 1 (collect requirements — run the workload, catalogue
capability gaps with evidence), lesson 2 (write the probes first, RED phase), lesson 4 (the loop,
measured). One gate: `PHASE-1-METHOD` (phase rows of the runbook carry one done-when each;
precedent: phase 0's single gate `PHASE-0-SCAFFOLD` covered its lesson whole). Passes when
`denver/gates/PHASE-1-METHOD.log` holds `RESULT PASS` with: every probe arm producing a clean
named verdict, workload-failure evidence captured, one hypothesis→verdict cycle measured under
an hour.

## The worked example, recovered

The plan cites `TAOXICHUAN_JVM_ATTEMPT_2026_11_09.md` — **not on branch `denver`** (same trap as
session 1's plan-cited shanghai README). Recovered from git history (`git show d7878378`), and a
fuller checkout of `main` lives at `/src/akuma-merge` (taoxichuan `docs/runbooks/gates.md` read
there). Its blocker table is the shape REQUIREMENTS.md must take:

| # | blocker | evidence |
|---|---|---|
| 1 | W^X: HotSpot's RWX `mprotect` refused (`EACCES` by design, `akuma-syscalls-mem::wx`) | `Error occurred during initialization of VM: Failed to mark memory page as executable` |
| 2 | `getcpu` served nowhere | `getcpu(2) system call not supported by kernel`; HotSpot `os::init_2` exits on `ENOSYS` |
| 3 | NULL store after CodeCache setup (undiagnosed) | `[Fault] EL0 … far=0x0`; needs a fault report that names the module |

Order note: on aarch64, W^X fired **before** getcpu. Blocker order is evidence, not plan.

## Denver-specific evidence already in hand (the workload on the best kernel)

`java -version` runs on **the host metal** (the ryzen, Akuma kernel `0.0.9 886dabf`, Alpine/musl
userland — `/usr/bin/java` → Corretto 21.0.13 alpine x86_64, 188.8 MiB, JRE-shape: `java, jfr,
jrunscript, jwebserver, keytool, rmiregistry`, **no javac/jar**). Result:

- **SIGSEGV, SEGV_MAPERR at `0xffffffffff600800` — the vsyscall page — 0.024 s into
  `JNI_CreateJavaVM`.** RIP *jumped into* the page (`RAX=RIP`); it is not mapped. Two hs_err
  artifacts: `hs_err_pid6587.log` (repo root, 2026-11-09, someone tried before) and a fresh
  capture this session (reproduced first-hand, 2026-11-13). **Gap #1 (new, x86_64-specific,
  not in the attempt's table): the kernel advertises no vDSO, and something in HotSpot falls
  back to the legacy vsyscall address → unmapped → SIGSEGV.**
- Hypothesis (to be *proven* by the auxv probe, not assumed): `AT_SYSINFO_EHDR` (33) is absent
  from Akuma's auxv — `crates/akuma-elf-exec/src/lib.rs` fills `AT_PHDR/AT_PHENT/AT_PHNUM/
  AT_BASE/AT_ENTRY` + brk only; **no `AT_SYSINFO_EHDR`, no vDSO anywhere** (grep over the crate;
  `docs/reference/subsystems/syscalls.md` and `kvm.md` mention neither vDSO nor vsyscall).
  musl itself degrades to raw syscalls without a vDSO (tokio works — phase 0 proved it); it is
  HotSpot's time path that dies on the fallback.
- **`getcpu` is unserved on x86_64 too**: `rg getcpu amd64/ crates/akuma-tokio-kernel*
  crates/akuma-syscalls*` → no hits; `docs/reference/subsystems/syscalls.md` has no getcpu row.
  The taoxichuan blocker 2 transfers as a hypothesis the probe must confirm on denver.
- W^X on this kernel generation: `userspace/jitprobe/README.md` (2026-10-04, kernel 0.0.8)
  says single-call W|X is refused `EINVAL` while RW→`mprotect(R+X)` **works**; the attempt doc
  (aarch64 tokio kernel) says `EACCES` by design (`akuma-syscalls-mem::wx`). Either answer is a
  gap for HotSpot's reserve-and-execute path; the `rwx` arm records which, not why.

## What is reusable as-is

| piece | path | use |
|---|---|---|
| probe shape (arms in forked children, named verdict lines, `JITPROBE OK/FAIL`) | `userspace/jitprobe/c/jit_probe.c` + `build.sh` + README | the `jvmprobe` skeleton; its `wx_mmap`/`rw_to_rx` arms are the `rwx` arm's precedent |
| probe runtime precedent (static musl, named `STEP PASS`, bounded, exit codes, `RESULT PASS`) | `userspace/tokioprobe/` (on branch; binary already proven in the phase-0 image) | line conventions + the guest-image placement (`/bin`) |
| rig + probe driver shape (free slot → boot → **ssh round trip** → probe → always stop) | `denver/vm-probe.sh`, rig at `/root/denver/w/` (D4) | `denver/probe-method.sh` is this script with new arms |
| image build trick | taoxichuan attempt doc: root via **`mke2fs -d`** from a staging tree | the JDK image; host has `mke2fs` 1.47.4 (`-d` supported), `debugfs` for extracting the proven 64 MiB image |
| toolchain | host gcc is Alpine/musl: `gcc -static` produced a static-pie musl ELF (verified this session) | probes build **on-box**, no rustup/musl-cross (both absent; `tokioprobe`'s `build-musl.sh` path is *not* runnable here) |
| syscall census reference | `taoxichuan/docs/runbooks/gates.md` (via `/src/akuma-merge`): `[nosys] nr=N` console lines are a checklist; `grep -a` mandatory | reading `vmrun.log` after the run |
| auxv filler | `crates/akuma-elf-exec/src/lib.rs` | names the AT-entries a vDSO gap would add (closing pass will extend *there*, R2 — never a fork) |

## What is missing

- The four probes of lesson 2 (`jvmprobe`, `getcpu`, `getauxval`, cycles-per-call) exist nowhere
  on this branch — `mwprobe` itself is on the Mac only (name cited in the plan; the style survives
  in `jitprobe`/`tokioprobe`).
- The JDK is not in any guest image; the phase-0 rig image is 64 MiB (busybox/sshd/uname/
  tokioprobe only).
- `denver/docs/REQUIREMENTS.md` does not exist (lesson 1's deliverable).
- No measurement of the hypothesis→verdict cycle exists (lesson 4's deliverable).

## Composition plan (new-LOC estimate)

1. `denver/probes/jvmprobe.c` — one static-musl C binary, named arms, each printing its own
   verdict line so phase-2 gates can grep per-name (`GETCPU RED ENOSYS`, `AUXV PASS/RED …`,
   `CYCLES …`, `RWX RED EINVAL/EACCES`, `JVM RED …` + captured `hs_err` reference). Arms run in
   forked children (a crash reads as `CRASH`, never kills the suite). Bounded: `alarm(30)` +
   `timeout` on the ssh side. ~250 LOC.
2. `denver/probe-image.sh` — extract the proven image (`debugfs rdump`), add the JDK tree
   (`/usr/lib/jvm/java-21-openjdk` → `/opt/jdk`) + probes + `/etc/resolv.conf` (tokioprobe's
   README requirement), `mke2fs -d` → `/root/denver/w/denver-jdk.img` (~576 MiB; rig stays
   outside the tree per D4). Writes `/root/denver/w/vm-jdk.spec` (copy of `vm.spec`, new disk).
   ~60 LOC.
3. `denver/probe-method.sh` — `vm-probe.sh` shape against `vm-jdk.spec`: free slot → boot →
   ssh round trip → run every arm + `java -version` → per-arm verdict lines → `METHOD PASS` /
   `RESULT PASS` (the driver's marker, D12 of phase 0's convention) / `RESULT FAIL: <why>` →
   always stop. ~90 LOC.
4. `denver/docs/REQUIREMENTS.md` — gap → evidence → closing pass, ranked; filled from the
   captured verdicts + both hs_errs + the attempt's blockers 2–3. ~80 lines.
5. Living lessons L1/L2/L4 + records (LOOP_LOG, RESULT, work log). ~150 lines total.

**Total new code ≈ 400 LOC, zero kernel changes** — phase 1 changes no kernel code on purpose
(R6 surface untouched; the R6 default build stays what phase 0 proved).

## Decisions

- **D9 — phase 1 is one gate, `PHASE-1-METHOD`.** The runbook's phase-1 row is one conjunctive
  done-when (requirements complete ∧ probes RED ∧ cycle measured); phase 0 set the single-gate
  precedent. Lesson gates stay distinct *inside* the record (three named deliverables).
- **D10 — probes are one C binary with named arms (the `jitprobe` shape), built with the host's
  Alpine gcc `-static`**, not four crates: `tokioprobe`'s cargo+musl-cross build path needs
  rustup and a musl cross-gcc (absent); plain C needs only the verified host gcc. Each arm
  prints a NAME-prefixed verdict line so later gates grep one name each (lesson 2: every gate
  is a probe — the phase-2 gates are already these lines).
- **D11 — workload evidence comes from the denver guest first, host metal second.** The
  attempt's A/B rule ("compare against a known-good kernel run") becomes: same Corretto build,
  host Akuma metal (hs_err captured, gap #1) vs denver guest (this session's run). Both kernels
  are Akuma — the guest adds the platform-stack answer, the metal the richest-syscall-surface
  answer.
- **D12 — the gate marker stays `RESULT PASS`** in `denver/gates/PHASE-1-METHOD.log` (driver
  convention; the method gate passes on *method* evidence: clean verdicts + captured failure +
  measured cycle, not on the JVM passing — RED is the expected phase-1 outcome).

## Risks

- The guest may OOM or hang running the JVM under 3 GiB — bounded by `timeout`/`alarm` on every
  arm; a hang is a verdict (`JVM RED timeout`), not a stall.
- `mke2fs -d` ownership/perms of the JDK tree must survive (host is root, image is root — same
  uid; verified by `debugfs` ls after build).
- Corretto alpine JRE is musl-linked; the denver guest runs musl dynamic binaries (phase 0
  image has `ld-musl` via sshd? — sshd runs, and tokioprobe is static). **If the dynamic loader
  is missing in the image, `java` dies before any kernel gap — that is itself evidence (a
  userspace gap, listed as such in REQUIREMENTS.md, closing pass: bake the loader).** The probe
  script captures `ldd`-equivalent (`/opt/jdk/bin/java -version` stderr verbatim) so the verdict
  is always readable.
