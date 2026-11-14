# Denver loop log — Part II: one entry per pass

**Voice: Record.** What we did (brief), what decision it forced, precedent cited (R9). Part I
(the lessons) lives beside this file in `denver/docs/lessons/`; the frozen plan is
`denver/docs/archive/KERNEL_HOWTO_BUILDING_CUSTOM_PLATFORM_KERNEL.md`; the procedure is
`denver/docs/runbooks/AUTORESEARCH.md`.

### PHASE-0-SCAFFOLD — 2026-11-13, session 1 (goose, glm-5.3-flash)

**What we did.** Oriented: the gate needs `akuma-jvm-kernel/` + `denver/` over
`akuma-tokio-kernel` — and neither the crate nor the branch's plan-cited files
(`crates/akuma-tokio-kernel/README.md`, `shanghai/README.md`) exist on branch `denver`; the
whole proven stack (13 new + 14 updated crates, the root/metal pairing, `akuma-runtime-tokio`)
lives on sibling branch `shanghai` (tip `d4a6c7d9`, 240 commits since merge-base `cf687e25`),
six gates green on this very machine (its `RESULT.md`). Research artifact first (R1):
`denver/docs/research/PHASE-0-SCAFFOLD.md`, decisions D1–D5. Then: merged `shanghai` into
`denver` (kept denver's planning docs; submodule pointers took shanghai's), added
`akuma-jvm-kernel` (1:1 feature-map pass-through, zero capabilities, `forbid(unsafe_code)`, the
linker-script relay one hop up) and `denver` (the README's three-file kernel, package name
`denver` = `uname -n`), root `members` += both, driver staging learned the root manifests (D5).
Builds green cold: `denver` 22.9s, `akuma-amd64` 38.0s, `taoxichuan` 15.8s — all
`x86_64-unknown-none` release, against the merged state, no code fixes needed anywhere
(composition, not porting: R2 paid off exactly as advertised). Boot probe: **blocked, escalated**
— the host was not booted with `kvmctl`, `/dev/kvm0` is ENOENT, no QEMU/Firecracker-viable
alternative exists on the box, and a reboot is beyond the plan's authority (the session runs on
the machine it would reboot). Escalation filed: `denver/ESCALATE.md`; capture:
`denver/gates/PHASE-0-SCAFFOLD.log`.

**What decision it forced.**
1. *Platform acquisition is a branch merge, not a port* — decided by the plan's own citations
   plus R2/R5 (a partial sync is fork-shaped); precedent: `proposals/B15_TOKIO_KERNEL_KIT.md`
   (the extraction these crates implement), shanghai `RESULT.md` (proof on this hardware).
2. *`akuma-jvm-kernel` is a pure pass-through* — decided by its own precedent one level down
   (`akuma-tokio-kernel`'s feature map onto its machine half; plan lesson 3: "the way `console`
   propagates today"); R6 keeps it off-by-default everywhere.
3. *The denver kernel is three files + a banner* — precedent: the README's `osaka` pattern;
   `shanghai/src/main.rs` (18 lines). Zero kernel code of our own.
4. *Rig and images live outside the tree* (`/root/denver/w/`) — precedent: shanghai `RESULT.md`.
5. *Driver staging must carry the root manifests* — a members-line gate commit without
   `Cargo.toml`/`Cargo.lock` is a tree that cannot build its own kernel; machinery precedent:
   the work log's driver-fix entries. Fixed in `denver/autoresearch.sh`, logged there too.
6. *Machine facts recorded as constraints, not fought*: the box's toolchain is standalone
   `/usr/local/rust` (1.101.0-nightly; no rustup, so `rust-toolchain.toml`'s pin is not
   enforced here) and carries no `aarch64-unknown-none` std — the aarch64 default build was
   never runnable on this host; the checkable R6 surface here is the x86_64 + host surface,
   which is green. Recorded in `RESULT.md` and the work log; nobody patched around it.

**Gate state:** NOT GREEN (build half proven, boot half pending the host reboot). NEXT GATE stays
on this gate until `RESULT PASS` appears in `denver/gates/PHASE-0-SCAFFOLD.log`.

### PHASE-0-SCAFFOLD — 2026-11-13, session 2 (goose, glm-5.3-flash; the redo pass)

**What we did.** Took the answered escalation (`ESCALATE_ANSWERED=1`; archive:
`denver/docs/archive/escalations/2026-11-13-PHASE-0-SCAFFOLD.md`): the human had rebooted the
ryzen into the kvmctl-armed kernel (GRUB entry 29, `kvmctl kvmmem=4096` — exactly the remedy
session 1 named), run the probe by hand over ssh, captured `RESULT PASS` into the gate log, and
committed it with the rig script as WIP `b0f1e325`. Research addendum first (R1): decisions
D6–D8 in `denver/docs/research/PHASE-0-SCAFFOLD.md`. Then the session's own first-hand rerun:
`sh denver/vm-probe.sh 2>&1 | tee -a denver/gates/PHASE-0-SCAFFOLD.log` — slot freed, guest
booted, ssh round trip answered `Linux denver 0.1.0-8dc2dbcc-denver`, tokioprobe
`RUNTIME PASS` (4 workers) / `UNAME PASS` / `HTTPS PASS` / `RESULT PASS`, guest stopped. One
machinery fix: the escalation archive step now never overwrites an earlier archive (work log,
finding 5). **Gate GREEN**; phase 0's done-when holds: Denver boots tokio, `uname` says denver.

**What decision it forced.**
1. *Accept operator-supplied machine state, then re-prove it first-hand* (D6) — the hand-run
   capture stands as evidence; the session's own capture proves the rig still green after it,
   so the gate's proof is not borrowed.
2. *Hidden device nodes are checked by opening, never by listing* (D7) — kvm.md says `/dev/kvm0`
   is invisible to `ls /dev` by design; this session initially re-read a bare `ls` ENOENT as
   "still not armed" and was corrected by the file evidence (`vmrun.log` carrying live guest
   cpuid/DNS lines). Lesson L3 records it; the check is the open or vm-run's own error text.
3. *The phase line stops the loop* — phase 0 ends with one recorded human intervention (the
   reboot, session 1), and the runbook's transition rule says a phase that needed interventions
   stops at its summary for human review ("autonomy is earned per phase"). Filed
   `denver/ESCALATE.md` (QUESTION: review the phase-0 summary); `NEXT GATE` stays on this gate
   with the recorded reason that it is already green — runbook step 2's "unless a recorded
   reason says otherwise" sends the next session to phase 1 (lessons 1/2/4) after the review.

**Gate state:** GREEN (`RESULT PASS` in `denver/gates/PHASE-0-SCAFFOLD.log`, first-hand session
capture, hand-run capture preserved above it). Phase 0 summary written in `denver/RESULT.md`;
loop stopped at the phase line for human review.

### PHASE-1-METHOD — 2026-11-13, session 3 (goose, glm-5.3-flash; opens phase 1)

**What we did.** Took the answered phase-0 review (archive: the escalation file; runbook step
2's "unless a recorded reason says otherwise"): the gate was green, the review decided phase 1
opens, so this session opened it. Research first (R1):
`denver/docs/research/PHASE-1-METHOD.md`, decisions D9–D12, with the worked example recovered
from git (`TAOXICHUAN_JVM_ATTEMPT_2026_11_09.md`, commit `d7878378` — not on this branch; the
same plan-cited-path trap session 1 hit, and a full `main` checkout exists at
`/src/akuma-merge`). The workload (`java -version`, Corretto 21 alpine musl x86_64 already on
the host) was run on BOTH kernels this rig can reach (D11): the metal leg crashed at the
vsyscall page (`hs_err_pid*.log`, pc=0xffffffffff600800, RIP inside it — a gap the aarch64
attempt never saw) and the guest leg died at W^X with the attempt's exact blocker-1 line
(`Failed to mark memory page as executable`) — reproduced first-hand on x86_64. Then the
deliverables: `denver/probes/jvmprobe.c` (260 LOC, one binary, named arms, jitprobe's shape;
built with the host's alpine musl gcc `-static` — tokioprobe's cargo+musl-cross path is not
runnable here), `denver/probe-image.sh` (the proven phase-0 root + the JRE + loader/libs, via
`mke2fs -d -E no_copy_xattrs`: the kernel has no xattr syscalls — machine fact), and
`denver/probe-method.sh` (metal leg + guest RED suite + timed H1 + hs_err evidence). Gate run:
**RESULT PASS** in `denver/gates/PHASE-1-METHOD.log` (third run; runs 1–2 died on the suite's
own parser bugs — lowercase grep, eval quoting, forked-child stdio vanishing under pipe
buffering — each a one-hypothesis fix, never a third). Deliverable: `denver/docs/REQUIREMENTS.md`
(G1 W^X → G2 getcpu → G3 vDSO → G4 procfs auxv/maps → G5 time cost, ranked by the JVM's init
order, suspects separated); living lessons L1, L2, L4 written.

**What decision it forced.**
1. *Phase 1 is one gate* (D9) — the runbook's phase-1 row is one conjunctive done-when; phase 0
   set the single-gate precedent. The three lesson gates stay distinct inside the record.
2. *Probes are one C binary with named arms* (D10) — precedent jitprobe; each arm's NAME-prefixed
   line is what the phase-2 gates grep. The vocabulary is fixed before any closing code.
3. *A/B both kernels* (D11) — the attempt's "Linux A/B" rule became metal-vs-guest with the same
   binary; the two kernels fail at DIFFERENT first gaps (metal: vDSO/vsyscall; guest: W^X), so
   REQUIREMENTS rank must follow each kernel's own evidence, not one list.
4. *The gate marker stays `RESULT PASS`, judged on method evidence* (D12) — RED is the expected
   phase-1 outcome; the gate passed on clean verdicts + captured failure + measured cycle, not
   on the JVM succeeding.
5. *Phase 1 needed zero interventions* → the transition rule lets this session open phase 2
   itself: `NEXT GATE: PHASE-2-WX` (G1, the first blocker in the JVM's own init order).
   Machine facts recorded, not fought: clock slew (negative spans clamped + logged), RWX
   baseline moved on metal 0.0.9 vs guest stack EACCES, no xattr syscalls, no vDSO anywhere.

**Gate state:** GREEN (`RESULT PASS` + `CYCLE PASS H1` in `denver/gates/PHASE-1-METHOD.log`,
first-hand capture). Phase-1 done-when met: REQUIREMENTS.md complete; every probe failed (or
passed, diagnostics arm) cleanly on both kernels; hypothesis-to-verdict cycle measured (0–1 s
clamped; whole method 9 s wall clock).

NEXT GATE: PHASE-2-WX

### PHASE-2-WX — 2026-11-13, session 4 (goose, glm-5.3-flash; opens phase 2)

**What we did.** Took the transition line (phase-1 D-carry: zero interventions → the agent
opens phase 2 itself). Research first (R1): `denver/docs/research/PHASE-2-WX.md`, decisions
D13–D16, precedents cited (R9): the canonical `TAOXICHUAN_WX_POLICY.md` (the `wx=0` escape
hatch — "restores the old behaviour (`PROT_EXEC` widens a mapping to RWX)"), git `0dcc33e7`
(taoxichuan later made W^X unconditional — a kernel *generation* decides its default policy),
`BUG_FIX_LIST`/`AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md` Fix 18 (the metal went the other way and
*grants* W|X for V8 — two sibling kernels already chose opposite policies, so a per-build
policy switch is the established shape), the attempt doc's policy note ("running any JVM here
means giving up W^X … a policy decision, not a bug"). Then the smallest change the plan allows
(~45 kernel LOC): the `wx-relaxed` feature — the pure checks in `akuma-syscalls-mem::wx`
answer `Allow` (host-tested both ways: 46 strict / 43 relaxed, all green), and `WX_ENFORCE` in
`akuma-tokio-kernel-amd64` boots `false`, which is the proven `wx=0` behavior path at compile
time; relayed 1:1 `denver` → `akuma-jvm-kernel` → `akuma-tokio-kernel` → the machine half (the
`console` naming rule; off by default at every level — R6, default builds unchanged, re-verified
`-p denver` and `-p taoxichuan`). Rig: a *second* kernel binary `denver-wx` + `vm-wx.spec`
(D15, D4's rule — the phase-0/1 rig stays byte-reproducible), `denver/probe-wx.sh` (vm-probe
shape, D16). Gate run: **RESULT PASS first capture** in `denver/gates/PHASE-2-WX.log` —
platform sanity (`tokioprobe` PASS under the relaxed policy), `RWX INFO mmap-RWX-allowed` +
`RWX PASS rw_to_rx=42` (the RED phase 1 captured on this same image, closed), and the workload
past the blocker-1 line. Diagnostics (R8, two appended boots in the same log): the movement is
a **raw SIGSEGV, zero output, no `hs_err`, no core, 3 boots reproducible**; the kernel names
the death but not the address (`[Fault] slot 5 killed (SIGSEGV)`); the console census proves
`getcpu` was **never called** (death precedes `os::init_2`; last ENOSYS calls: `setitimer(38)`,
`membarrier(324)`, `sysinfo(99)`, `prlimit64(302)`, `getrlimit(97)`, uid/gid family);
`-Xlog:all=info` wedges the guest; bare `ls` is not on the guest ssh PATH (`/bin/busybox`).

**What decision it forced.**
1. *The relaxation is one feature flipping the default of the existing hatch* (D13/D14) — not
   new enforcement logic and not a runtime flag: `wx=0` already flips every enforcement point,
   so the feature is "the hatch at compile time". The pure crate's flip is its host-tested
   policy statement (R3); the machine half rides the same feature name; no `wx=1` exists (R7).
2. *The gate kernel is a second rig binary, not an overwrite* (D15) — the RED gate stays
   reproducible, D4's rule applied to the kernel side.
3. *The rank re-ranked on evidence, openly* (recorded in REQUIREMENTS.md): closing G1 moved
   the first blocker to an unnamed SIGSEGV that fires before `getcpu`; the runbook's
   diagnostics-before-bugs rule and the attempt doc's own next step ("the fault handler
   printing the VMA") insert **PHASE-2-FAULTNAME** (G6: the user-SIGSEGV delivery path prints
   rip+cr2 with the `[Fault]` line — the aarch64 sibling's `ec=`/`far=` shape is the precedent,
   the amd64 line is the site) ahead of G2. G2 stays open (the arm still REDs; HotSpot exits
   on it later), G4 stays ahead of G3.
4. *`membarrier(324)` ENOSYS on the amd64 machine half* — found while reading the census: the
   pure decision (`akuma-syscalls-mem::membarrier`) exists, the arm is not wired there. A
   composition hole (R2), filed for the G2/PROCFS sessions' research, not fixed here.
5. *Zero interventions again* — autonomy holds into the next pass.

**Gate state:** GREEN (`RESULT PASS` + `RWX PASS rw_to_rx=42` in `denver/gates/PHASE-2-WX.log`,
first-hand capture, gate run + diagnostic boots in one file). Kernel-change cycle measured
(L4): build `--features wx-relaxed` 24 s, rebuild+stage+boot+verdict ≈ 4 min; the whole gate
from WIP commit to capture ≈ 11 min.

NEXT GATE: PHASE-2-FAULTNAME

---

### PHASE-2-FAULTNAME — 2026-11-13 22:48–23:04 UTC, session 5 (goose, glm-5.3-flash) — GREEN (closes G6; guest-proves G3)

**Goal:** close G6 (`denver/docs/REQUIREMENTS.md`): the workload's post-G1 death must be
*named* — the row demanded the user-SIGSEGV delivery path print rip+cr2 with the `[Fault]`
line, first-hand in `denver/gates/PHASE-2-FAULTNAME.log`.

**Finding 0 (what decision the research forced):** the artifact (written first, R1)
corrected this log's own PHASE-2-WX record before any run: the console census
(`/root/denver/w/vmrun-diag.log`) already carried the naming line — `[Fault] ring3 PageFault
rip=0xffffffffff600800 rsp=0x00006000014fe768 cr2=0xffffffffff600800 code=0x14` — one line
above the slot line that record quoted, and `git log -L` shows the line in `aafbb973`, the
kernel file's first commit. "The kernel names the death, not the address" was a misquote; the
real gap was the *capture* (probe scripts carried ssh-side output only) and the
address→module step. Decision: **zero kernel/crate LOC** (D17) — the gate runs the proven WX
rig unchanged (D18: uname must carry `0.1.0-8a658723-denver`, mismatch = FAIL) with a probe
that windows the console log per arm and names the module at ABI level (D19).

**Commands and outcomes** (capture: `denver/gates/PHASE-2-FAULTNAME.log`; two gate runs, both
RESULT PASS — run 1's interpretation line was false, see Interventions; run 2 is the
record's evidence):

| step | command | outcome |
|---|---|---|
| research first (R1) | `denver/docs/research/PHASE-2-FAULTNAME.md` (Finding 0, D17–D20) | precedents: the attempt doc's own next step — "a fault report that names the module (maps content, or the fault handler printing the VMA)" (git `d7878378`); the metal's fault-naming channel (`akuma-exceptions` `[SIGSEGV-HEAP] far= elr=`); `trace-err`'s `mm_dump_for` considered and rejected (slab-history ring, not the map; prints nothing for a never-mapped page) |
| the probe | `denver/probe-faultname.sh` (probe-wx shape + epoch stamps + console window) | parsing dry-run verified against `vmrun-diag.log` (busybox grep/sed/cut) before booting; `sh -n` is the build check for a shell-only delta |
| gate run 1 | `sh denver/probe-faultname.sh 2>&1 \| tee denver/gates/PHASE-2-FAULTNAME.log` | **RESULT PASS**, death named: rip=cr2=`0xffffffffff600800` code=0x14 — but the module branch printed a false SURPRISE (Interventions) |
| gate run 2 (record's evidence) | same, after a one-line fix | **RESULT PASS**: `FAULT PASS rip=0xffffffffff600800 cr2=0xffffffffff600800 code=0x14 module=legacy-vsyscall-page`; fault line byte-identical across runs (deterministic death); run 1's capture overwritten by `tee` — noted here |

**Verdict.** Gate GREEN: the death is named in the capture, first-hand, twice —
`rip==cr2=0xffffffffff600800`, `code=0x14` (user instruction fetch, not-present): HotSpot
jumps to the legacy vsyscall page because no vDSO exists (`AT_SYSINFO_EHDR` missing, phase 1)
— the metal `hs_err` pc, now guest-proven. G6 closed without kernel code. REQUIREMENTS
re-ranked openly (D20): **G3 (PHASE-2-VDSO) → G2 (PHASE-2-GETCPU) → G4 (PHASE-2-PROCFS) →
G5** — the vDSO gap is the proven first blocker; `getcpu` sits behind a death that precedes
it; maps stay behind `getcpu` because the next *unknown* (the attempt's blocker-3 NULL store)
arrives only after init advances.

**Interventions:** one. The probe's module branch compared a prefix-less hex capture against
the `0x`-prefixed constant, so run 1 — already RESULT PASS — printed `module=unmapped` plus a
false SURPRISE line. A probe whose interpretation can lie is a broken probe (L2's own rule,
its parse-time sibling); fixed in one line and re-run, and the overwrite of run 1's capture
is recorded here rather than hidden. Machine facts recorded (not fought): the console already
streams to `$W/vmrun.log` — the fix was reading it with attribution (byte offsets into the
log), not new plumbing; boot→ssh round trip ≈ 8 s on this box, the loop's floor so far.

**Cycle measured** (L4, named): research→GREEN ≈ 35 min, of which each gate run ≈ 2 min
including boot; per-step stamps are in the capture this time (L2 debt 2 paid), so the numbers
are measured, not reconstructed. Stamps: wall clock 11 s / 7 s post-boot for runs 1–2.

**Wall clock:** ~16 min (22:48–23:04 UTC). **Token spend:** the driver log is the ledger.

**Transition.** Zero kernel interventions → the ladder's first blocker is now a *named,
guest-proven* ABI gap, and the next pass is design work, not diagnostics: `NEXT GATE:
PHASE-2-VDSO` (G3: research first — a minimal vDSO page + `AT_SYSINFO_EHDR` in
`akuma-elf-exec` vs. provably serving the legacy fallback address; the gate arm already
exists: `AUXV RED AT_SYSINFO_EHDR missing` from phase 1, expected to flip to `AUXV PASS` with
the java arm past the vsyscall death).

NEXT GATE: PHASE-2-VDSO

### PHASE-2-VDSO — 2026-11-13 23:37–2026-11-14 00:5x UTC, session 6 (goose, glm-5.3-flash) — GREEN (closes G3; the ladder jumps two rungs)

**Goal:** close G3 (`denver/docs/REQUIREMENTS.md`): the workload must get past the
guest-proven death at the legacy vsyscall page (`rip=cr2=0xffffffffff600800 code=0x14`,
FAULTNAME) — the row's two candidates: a minimal vDSO + `AT_SYSINFO_EHDR` vs. serving the
fallback address.

**What the research forced (R1, `denver/docs/research/PHASE-2-VDSO.md`, Findings 0+1,
D21–D26):** first-hand disassembly over recall, both directions.

- *Finding 0 (HotSpot):* the metal `hs_err` frame (`V [libjvm.so+0xaef710]`,
  `RAX=RIP=0xffffffffff600800`) disassembles to **HotSpot hardcoding the legacy getcpu slot
  as a sign-extended imm32 and calling it as a C function** — the auxv key plays no role on
  that path, and a full objdump sweep of the JRE finds exactly one vsyscall reference. The
  transition line's "vDSO + auxv would unblock the workload" assumption was wrong before any
  code: the *page* is the consumer interface.
- *Finding 1 (musl — paid for with a RED):* the first build disclosed the non-ELF stub page
  as `AT_SYSINFO_EHDR`; **tokioprobe — no JVM anywhere — died with SIGSEGV**. The bisect
  (one variable per boot): map-only → PASS; pair-only → SEGV, named by the console
  (`cr2=0xffffffffff600020`, rip inside tokioprobe's own text). Disassembly of the fault site
  (`tokioprobe+0x179dd3`): **musl's static-PIE startup seeks key 33 by name (`cmp $0x21`)
  and parses the value as an ELF with no magic check** — e_phoff/e_phnum, phdrs for
  PT_LOAD/PT_DYNAMIC, dynamic entries to DT_NULL, then symbol tables or a clean
  "no vDSO symbols" return. Finding 0's "musl ignores the key" stands corrected in the
  artifact; the correction is itself the session's lesson about recall vs. binaries.
- *The ship shape (D21/D22-revised):* two pages, two honest objects. The legacy stubs
  (`gettimeofday/time/getcpu`, `mov nr,%eax; syscall; ret`, `0xcc` padding) mapped user-RX at
  the ABI-fixed `0xffffffffff600000` — HotSpot's interface, not disclosed; a **valid,
  exports-free vDSO ELF** one page below (`DT_NULL`-only dynamic array — every resolver's
  lookup cleanly returns "not found" and falls back to syscalls) disclosed as
  `AT_SYSINFO_EHDR` at `0xffffffffff5ff000`. One boot-time `paging::map_page` call into the
  kernel root per page; the PML4[511] widen (present-entry widening in
  `akuma-pte-amd64::step_down`) keeps kernel-image siblings `U=0`-guarded; `copy_kernel_half`
  hands both to every task — zero per-exec cost (D23). Feature `vsyscall`, off by default,
  relayed 1:1 down the five-crate chain (D25, the wx-relaxed shape); rig = `denver-vdso` +
  `vm-vdso.spec` (D15's second-rig rule); probe = `denver/probe-vdso.sh` (D26, faultname
  shape).

**Commands and outcomes** (capture: `denver/gates/PHASE-2-VDSO.log`; three judged runs —
run 1 RED on the tokioprobe sanity arm (Finding 1's design defect), the bisect boots
un-judged diagnostics, runs 2–3 GREEN with run 3 as the record):

| step | command | outcome |
|---|---|---|
| research first (R1) | `denver/docs/research/PHASE-2-VDSO.md` | Findings 0+1, D21–D26; precedents: the metal loader's 14-entry auxv + its own `hs_err` (absence precedent), `read_syscall_cost.c`/`AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md` ("no vDSO on Akuma" named as a class), `rg -il 'vsyscall\|vdso\|AT_SYSINFO'` over shanghai/taoxichuan/tokio-kernel → zero hits (nothing to copy), the attempt doc's blocker 2 (same gap, arch-dependent interface) |
| kernel delta | `vsyscall` feature, ≈70 LOC | default builds byte-unchanged (denver 23.0 s, taoxichuan 22.6 s re-verified post-Finding-1); clippy clean on the delta (one pre-existing `akuma-exec` warning, untouched) |
| gate run 1 | `sh denver/probe-vdso.sh …` | **RED**: tokioprobe SEGV — the auxv disclosure was a non-ELF page; Finding 1 born |
| bisects (un-judged, one variable per boot) | map-only / pair-only | map innocent; pair guilty — the console named the death (G6's capability paying for itself immediately) |
| gate run 2 | same | **RESULT PASS** but a false SURPRISE line: the probe's hand-computed expected constant disagreed with the kernel's correct value (INTERVENTION) |
| gate run 3 (record) | same, constant fixed | **RESULT PASS** clean: rig `0.1.0-e0924225-denver` (WX hash absent), boot line names both pages, tokioprobe `RESULT PASS`, `RWX PASS rw_to_rx=42`, `AUXV PASS AT_SYSINFO_EHDR=18446744073699061760`, java arm **no signal, no vsyscall fetch** — `exit=1` at a NEW blocker: `Too small maximum heap` |

**Verdict.** Gate GREEN — G3 closed. The ladder moved **two** rungs: past the vsyscall death
*and* past the getcpu exit point, exactly as D24 predicted (census: `nr=168` answered ENOSYS
through the stub and init advanced; HotSpot's `cmp $-1` wrapper reads the pre-zeroed cpu → 0).
The first fatal is now **G7** (new row): `sysinfo(2)` unserved → `_SC_PHYS_PAGES` fails →
HotSpot heap sizing exits cleanly with `Too small maximum heap` — the smallest arm in the
table (one syscall, validate + `copy_to_user` + the guest's real RAM). `epoll_pwait2` (229)
and `getrandom` (309) ENOSYS'd later and are held as suspects (G8), not gaps. Re-rank
recorded openly in REQUIREMENTS: **G7 (PHASE-2-MEMINFO) → G2 → G4 → G5** (G5's real-vDSO
payoff now has a live consumer: musl's own resolver).

**Interventions:** one. The probe's expected-value constant for the auxv arm was computed by
hand and was wrong; run 2 — already RESULT PASS — printed a false SURPRISE line against a
correct kernel. Same defect class as FAULTNAME's run-1 (a probe whose interpretation can lie
is a broken probe), different limb: this time it was the *constant*, not the parse. Fixed
with a python-checked constant and re-run; `tee` overwrote run 2's capture, recorded here
rather than hidden. The two bisect boots are not interventions — they are the stall
protocol's one-variable-per-verdict discipline working as designed. No operator
intervention was needed; autonomy holds.

**Cycle measured** (L4, kernel-change cycle, named): edit→build ≈ 20–23 s
(`--features wx-relaxed,vsyscall`); build→stage→boot→first verdict ≈ 3–5 s (boot→ssh round
trip is now the floor, faster than WX/FAULTNAME's ≈ 8 s); full gate run ≈ 7 s; research→GREEN
including the RED-1 design verdict, two bisect boots, and the constant fix ≈ 80 min wall
clock (stamps in the capture; the bisect boots' verdicts live in this entry and RESULT.md).

**Wall clock:** ~80 min (23:37–00:5x UTC). **Token spend:** the driver log
(`/tmp/denver-autoresearch.log`) is the ledger.

**Transition.** Zero kernel interventions; the next pass is the ladder's smallest arm:
`NEXT GATE: PHASE-2-MEMINFO` (G7 — serve `sysinfo(2)` behind the default-off feature map so
`_SC_PHYS_PAGES` answers and HotSpot sizes the heap; expected movement: the JVM past heap
sizing toward whatever G8's suspects actually do).

NEXT GATE: PHASE-2-MEMINFO

### OPERATOR-IMPROVEMENTS — 2026-11-14, session 7 (operator-directed merge verification + loop improvements; no agent gate run)

**Goal:** record the operator's merge of `litter/main` into `denver` and its verification,
land the quota-efficiency changes the six sessions measured, and re-rank from the one
behaviour change the rebuild exposed. No gate was run by an agent this session; the driver
stayed stopped.

**The merge verification (captures `/root/denver/merge-verify/`, kernels rebuilt from
3531cf69 and staged):** WX gate `RWX PASS rw_to_rx=42` + tokioprobe PASS on uname
`0.1.0-3531cf69-denver`; VDSO gate `RESULT PASS`, `AUXV PASS
AT_SYSINFO_EHDR=18446744073699061760` (gate verdict: MOVEMENT PASS). FAULTNAME gate
`RESULT FAIL: rig changed` **by design** — it pinned the historical wx binary hash
8a658723, which no rebuild can carry; not a regression. What was NOT verified:
`sysinfo(2)` on the merged tree — G7's clean `Too small maximum heap` exit is simply gone
from the java arm, with no sysinfo probe run to say why (RESULT.md has the full ledger).

**The behaviour change (the new evidence):** on the merged tree the java arm dies with
`[Fault] ring3 PageFault rip=0x000000003005fc41 rsp=0x00006fffffffba60 cr2=0x0 code=0x6`
— a user-mode write to NULL (code=0x6), past the heap blocker: the attempt's blocker-3
NULL store, a suspect since phase 1, is now the observed first blocker. Filed as **G9**.
A bare rip cannot be fixed: no maps exist to attribute it, so the runbook's
diagnostics-before-the-unknown rule applies a third time. **New order (REQUIREMENTS.md):
G4 (PHASE-2-PROCFS) → G9 (NULL-store fix, named once PROCFS lands) → G7 (demoted to
verifying sysinfo on the merged tree) → G2 → G5; G8 still suspects.**

**Probe debt paid:** `denver/probe-faultname.sh` accepted only the pinned rig hash — the
rebuild FAILed the gate on exactly that. Its identity check now accepts any denver build
(`0.1.0-*-denver`); the `wx-relaxed` policy itself is still proven by the unchanged
`RWX PASS rw_to_rx=42` arm, so the pass condition is not weakened.

**Loop efficiency landed (measured over six sessions: ~98% of input tokens are cache
reads, cost = context x calls):** `denver/bookkeep.sh` (checks, `--fix` for the
`##`-heading trap that bit session 5, `--digest` STATE.md, `--cost` COST_LEDGER.md);
driver changes in `denver/autoresearch.sh` (drift preflight, heading repair, bare-metal
`cargo build` for kernel crates that cannot host-test, `--max-turns 150` /
`--max-tool-repetitions 5` env-overridable with compaction OFF by default, post-session
digest+cost); `denver/docs/EFFICIENCY.md` (the knob ledger + the one-experiment-per-phase
rule); `denver/docs/STATE.md` and `denver/docs/COST_LEDGER.md` seeded. Sessions now
orient from the digest instead of cat-ing 39k of LOOP_LOG.

**Interventions:** none by the agent; the merge itself was operator-directed and is
recorded in RESULT.md with what it verified and what it did not.

**Transition.** The ladder's next rung is diagnostic, per the re-rank:
NEXT GATE: PHASE-2-PROCFS

### PHASE-2-PROCFS — 2026-11-14 06:00–06:50 UTC, session 8 (goose, glm-5.3-flash) — GREEN (closes G4; the fault can be named now)

**What we did.** Second session on the gate. Session 7 had done the research (R1), the
kernel-side D27–D29 work (walk moved to `akuma-exec`, auxv retention + `render_auxv`,
feature relay, "kernel builds green default+gate"), the staged rig (`denver-procfs` +
`vm-procfs.spec`, D30), and the D31 probe — whose first execution (05:00:32 UTC) FAILed
**on a probe defect, not a rig defect**: it defined `VDSoHASH` but compared
`$MERGEDHASH`, and the unset variable's empty pattern matched any uname (the gate log
of that run is preserved as `PHASE-2-PROCFS.typo-fail.log`). This session: fixed the
naming + added a fail-fast guard, ran the probe — six of seven arms GREEN
(tokioprobe, RWX ride, stack-auxv disclosure, **procfs-auxv with stack agreement**,
and the abort: `MAPS RED /proc/self/maps errno=2` — maps still absent while auxv
worked, *through the same hook table*), diagnosed that bisect without another boot,
fixed the kernel, re-ran, captured `RESULT PASS` (guest `0.1.0-f0dcacb6-denver`,
wall 188 s, stamped).

**The decision the failure forced (D32).** `errno=2` on maps = `pid_map_rows` returned
`None` = the file *absent*, the glue's own absence rule. Auxv working through the same
table proved the registration and the feature were live — the divergence was the
**address-space ownership model**: the moved walk (D27) resolves the metal's non-shared
*owner* and rejects shared views (the freed-tables hazard, PROC_MAPS_WALK_FREED_TABLES),
but the tokio kernel's `disown_space` makes **every** registered `Process` a
`new_shared` borrowed view of a root the kernel itself owns (`OWNED_ROOT` per slot,
parked-not-freed at exec) — so the owner resolution answered `None` for every pid. Two
layers of the walk (the owner lookup *and* the `is_shared()` re-check) assumed the
metal. Fix: a sibling walk `pid_map_rows_of_kernel_owned_roots` (leader's regions, pid's
own view, the same l0 re-check under `with_process`'s exclusive slot hold), registered
by `akuma-runtime-tokio` under `procfs-diag` — the model knowledge stays in the kernel
stack that owns it; no model flag, no new hook table (R7). The metal keeps
`pid_map_rows` verbatim; its build re-verified green, default build re-verified green
(R6). Precedents searched before deciding (R9): the hazard doc itself, `disown_space`/
`OWNED_ROOT` (proc.rs:668–691), `pid_auxv_of`'s tgid fallback (the runtime-side
model-specific policy precedent), `kernel-glue`'s by-design stub, and the arm64
`address_space_owner_pid_for_fault` (resolve by what the running state says when the
map can't name an owner).

**What GREEN means, honestly.** G4's files serve: 10 maps lines including the
executable image rows; auxv 15 pairs agreeing with the stack vector byte-for-byte on
`AT_SYSINFO_EHDR`. The NAME arm's *diagnostic window* worked — both runs' console shows
the workload dying at the exact expected rip (`0x3005fc41`, cr2=0, code=6) while the
poll ran — but its **attribution report cannot cross the guest's sshd**: java startup
plus the 2 ms maps poll starves sshd within seconds and it never recovers (reproduced
standalone; nine reconnect stalls). So `RESULT PASS` rests on conditions 1–6 + the
console fault recurrence; the rip→module snapshot is **still to capture**, and G9's
first job is exactly that (slower poll or console-side path — REQUIREMENTS G9 row
updated with the gate name).

**Machine facts recorded (not fought):** host toolchain — `/usr/local/rust/bin`'s
nightly is shadowed by system Alpine rustc unless PATH is fixed (the driver does this);
host tests need `--target x86_64-unknown-linux-musl`; `akuma-exec`/`akuma-mmu` are
bare-metal-build-only; the default and gate kernel builds share one output path — stage
default-first, gate-last, verify by size (gate ≈ +29 KB). Guest console on
`$W/vmrun.log` carried the fault lines with attribution offsets again (the
FAULTNAME/ VDSO capture pattern, reused unchanged).

**Interventions:** three, all in RESULT.md — the probe's name mismatch (false FAIL on a
healthy rig), the toolchain PATH/target facts, and the NAME-arm ssh starvation
(probe-side cadence, not a kernel defect).

**Cycle measured** (L4, third measurement, stamped): complete probe run 188 s; edit→
gate-build ≈ 1 min; build→stage→PASS ≈ 3 min; the session's first two probe
executions were aborted-but-informative (constant defect; model divergence) — the third
was the first complete run and it was GREEN.

**Transition.** G4 closed; the ladder's next rung is the fix the diagnostics were built
for — the rip has a kill site but no owner yet:
NEXT GATE: PHASE-2-NULLSTORE

### PHASE-2-NULLSTORE — 2026-11-14, sessions 9–10 (research+probe, run to PASS) and session 11 ~08:50–09:00 UTC (records + ride-along) — GREEN (G9 named to first order; fix pass becomes PHASE-2-NULLFIX)

**What we did.** Session 9 wrote the research (R1) and landed the console-side attribution
(D33: `fault_attribution` at the ring3 kill path — map rows around rip, the GPR file,
stack qwords — plus D34, the `trace-err` relay, both machine-crate, off-by-default).
Session 10 built the probe (D35/D36: the `mmaparms`/`sysinfo` java-free arms, the seven
pass conditions) and ran it to `RESULT PASS` (guest `0.1.0-800e8c80-denver`, wall 43 s:
rig identity, tokioprobe, RWX ride, MAPS 12 lines / AUXV 15 pairs agreeing on
`AT_SYSINFO_EHDR`, MMARMS contract facts, SYSINFO recorded, **NAME PASS** — the
kernel-side map row covers the python-checked rip and the host arithmetic names ld-musl
fstatat's copy-out) — and was cut off before its records. Session 11 opened on exactly
that state: evidence in, bookkeeping missing. STATE.md + the gate log + git reconstructed
the narrative without one whole-file read (the digest did its designed job); no probe
re-run was needed — the log's guest stamp is HEAD and its NAME lines match the
working-tree probe's format, so the evidence is current by construction.

**The gate's payoff, read end-to-end (D38).** The console window's ring line is the death
explained: musl's fstatat ran the raw `stat` (nr=4) with a REAL buf
(`a2=0x6fffffffba80`) — SUCCESS — then its own copy-out faulted at the `rep stos` with
rdi=0. r8/r9/r10 were zeroed across the syscall. The `regpreserve` canary arm (staged
into the image at 07:57, never dispatched by the passing script; session 11 booted the
gate build once and ran it java-free) confirmed the class: lazy faults and plain syscalls
keep every register canary; **the kernel copying syscall results into an unfaulted user
page clobbers ALL six callee regs**, restoring the syscall's *internal* marshaled
arguments (`rdi=0xffffffffffffff9c`=AT_FDCWD, `rdx`=the buf). G9's NULL store is a
**trap-frame preservation bug on the syscall-internal fault-retry path** — not a
userspace NULL, not the mmap contract. Also in the log, recorded-not-judged: mmap W+X
encodes never-writable here (Linux honors it) — a fix candidate for the suspects list,
unrelated to G9's cr2=0 death.

**The decision the measurement forced.** D37 offered the fix-in-this-gate branch only for
"a small, precedented contract fix"; what the evidence named is the frame-handling core
of the syscall dispatch — not small, and not yet bisected (the arm's shape-(b) vs shape-(c)
split is the axis: the same stat with a pre-faulted buf page keeps regs if the
fault-inside-copy-out is the trigger). Honest branch taken: GREEN = named to first order;
the fix is the next pass's opening job. **NEXT GATE: PHASE-2-NULLFIX** — (1) bisect the
clobber path in `akuma-tokio-kernel-amd64`'s dispatch; (2) fix behind an off-by-default
feature relayed 1:1 (R6), metal untouched; (3) regpreserve all-REGPASS; (4) the java arm
MOVES or the fix isn't one (D37). SYSINFO `errno=38` in the same log feeds G7: main's
merged changes did not serve sysinfo — PHASE-2-MEMINFO stays on the ladder behind the fix.

**Machine facts recorded (not fought):** an arm staged into the image but never dispatched
by the probe script is dead evidence — dispatch every arm you stage, or run it standalone
(java-free arms cross the wire even while java stalls nothing). The guest ended on its own
after the ride-along's ssh timeout; the first `echo stop` EINVAL'd *because* it was already
gone — re-issued, zero vm processes remained, no orphaned slot. And the commit-hygiene
catch: session 10's kernel-side refinements to D33 (stack window 64→256 B in four lines;
`ring_dump_slot` newest-first, capped — the gate log's format matches this exact state)
were left uncommitted, so the passing kernel was built from the working tree, not HEAD;
session 11 verified both builds green (gate feature set 27.9 s, default 26.9 s, lints
clean) and committed source and evidence together — the proof and the code it ran on now
share a commit.

**Interventions:** two, both in RESULT.md — the cut-off session's records (reconstructed,
not re-run), and the staged-but-never-dispatched arm.

NEXT GATE: PHASE-2-NULLFIX
